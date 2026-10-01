#!/bin/sh
# git-http-backend for nginx (through fcgiwrap), plus one convenience: a request for a repository
# of an owner in AUTO_CREATE_OWNERS creates it first, empty, on branch main, with pushes
# enabled. That is what the GitHub mock's "create a repository" needs: it answers with a clone
# URL here, and the repository must exist when the coder first looks at it (as one that GitHub
# has just created exists, with no branch). Any other missing repository is a 404, as before.
#
# nginx sets GIT_PROJECT_ROOT and PATH_INFO=/<owner>/<repo>.git/<rest> (the owner and the
# repository name are `[A-Za-z0-9._-]+`: nginx.conf matched them).
set -eu

root=${GIT_PROJECT_ROOT:-/srv/git}
path=${PATH_INFO#/}
owner=${path%%/*}
repo=${path#*/}
repo=${repo%%/*}

allowed=$(cat /tmp/auto-create-owners 2>/dev/null || true)
for candidate in $allowed; do
  if [ "$candidate" = "$owner" ] && [ ! -d "$root/$owner/$repo" ]; then
    case "$repo" in
      *.git)
        mkdir -p "$root/$owner"
        # `git init` is safe to run twice at once: both end up with the same repository.
        git init -q --bare -b main --template= "$root/$owner/$repo"
        git --git-dir="$root/$owner/$repo" config http.receivepack true
        git --git-dir="$root/$owner/$repo" config http.uploadpack true
        ;;
    esac
  fi
done

exec /usr/libexec/git-core/git-http-backend
