#!/bin/sh
# What git needs in a run pod (ADR 0041), with real git and real mounts, no cluster.
#
# A run pod mounts its own run's directory read-write, a worktree's mirror READ-ONLY and the worktree's own
# directory in the mirror (<mirror>/worktrees/<name>) read-write on top, each at the path the coder has it at, and
# nothing else of the volume. This builds that view in a private mount namespace (bind mounts, as the container
# runtime makes them from the pod's subPath mounts) and runs git as uid 10001 in it:
#
#   * what a repository's checks do with git works: status (also after the working tree changed: the index is
#     refreshed in the worktree's own directory), log, diff, describe, rev-parse, grep, switching HEAD;
#   * what writes an object, a ref or the configuration of the shared mirror fails: add, commit, stash, reset, tag,
#     branch, config, and a write to the mirror's config, hooks, refs, objects or worktrees/;
#   * nothing of another run, of meta/ or of another mirror is there, and a scratch project (its own repository
#     inside the run's directory) commits.
#
#   sudo sh deploy/coder/tests/git-in-run-pod.sh        (from the repository root; needs root, unshare and setpriv)
#
# Without a mount namespace it says so and passes, unless ADAM_TEST_REQUIRE_MOUNTS=1 (CI sets it).
set -eu

if [ -z "${ADAM_GIT_POD_INNER:-}" ]; then
  if [ "$(id -u)" -eq 0 ] && unshare -m --propagation private true 2>/dev/null && command -v setpriv >/dev/null; then
    exec env ADAM_GIT_POD_INNER=1 unshare -m --propagation private sh "$0" "$@"
  fi
  if [ "${ADAM_TEST_REQUIRE_MOUNTS:-}" = 1 ]; then
    echo "FAIL: no mount namespace (needs root, unshare and setpriv), and ADAM_TEST_REQUIRE_MOUNTS=1" >&2
    exit 1
  fi
  echo "skipped: needs root, unshare and setpriv (run it with sudo)"
  exit 0
fi

fail=0
volume=$(mktemp -d)
mkdir -p /work
trap 'umount -R /work 2>/dev/null || true; rmdir /work 2>/dev/null || true; rm -rf "$volume"' EXIT
chmod 755 "$volume"
chown 10001:10001 "$volume"

as_pod() { setpriv --reuid=10001 --regid=10001 --clear-groups env HOME=/tmp GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
  GIT_AUTHOR_NAME=a GIT_AUTHOR_EMAIL=a@example.test GIT_COMMITTER_NAME=a GIT_COMMITTER_EMAIL=a@example.test \
  sh -c "$1"; }
# ok <description> <command>: it succeeds in the pod. no <description> <command>: it fails.
ok() { if as_pod "$2" >/dev/null 2>&1; then echo "ok   $1"; else echo "FAIL $1"; fail=1; fi; }
no() { if as_pod "$2" >/dev/null 2>&1; then echo "FAIL $1"; fail=1; else echo "ok   $1"; fi; }
equals() { if [ "$(as_pod "$2" | tr '\n' ' ')" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1"; fail=1; fi; }
says() { if as_pod "$2" 2>&1 | grep -Fq -- "$3"; then echo "ok   $1"; else echo "FAIL $1"; fail=1; fi; }

# --- the volume, as the coder leaves it, seen from the coder at /work
mount --bind "$volume" /work
# shellcheck disable=SC2016  # $mirror is the pod shell's to expand
as_pod '
set -eu
mirror=/work/git/example.test/o/r.git
mkdir -p /work/git/example.test/o /work/git/example.test/p /work/workspaces/a /work/workspaces/b /work/meta/b
git init -q --bare -b main "$mirror"
git init -q --bare -b main /work/git/example.test/p/other.git
git init -q -b main /tmp/git-in-run-pod-seed
cd /tmp/git-in-run-pod-seed
echo hi > README.md
git add README.md
git commit -qm seed
git tag -a v1.0 -m release
git push -q "$mirror" main v1.0
cd "$mirror"
git branch --no-track agent/a main
git branch --no-track agent/b main
git worktree add -q /work/workspaces/a/r agent/a
git worktree add -q /work/workspaces/b/r agent/b
echo private > /work/workspaces/b/r/private.txt
echo "{}" > /work/meta/b/r.json
git init -q /work/workspaces/a/scratch
echo s > /work/workspaces/a/scratch/s.txt
rm -rf /tmp/git-in-run-pod-seed
'
mirror=/work/git/example.test/o/r.git
# git names the directory of a worktree in the mirror as it likes (r, r1, ...): read it from the .git file.
admin=$(sed -n 's/^gitdir: //p' /work/workspaces/a/r/.git)
case "$admin" in "$mirror"/worktrees/?*) ;; *) echo "FAIL the worktree's directory in the mirror is $admin"; exit 1 ;; esac

# --- the run pod's view of run a: a tmpfs for the image's /work and the three mounts, parents first
umount /work
mount -t tmpfs tmpfs /work
mkdir -p /work/workspaces/a "$mirror"
mount --bind "$volume/workspaces/a" /work/workspaces/a
mount --bind "$volume/git/example.test/o/r.git" "$mirror"
mount -o remount,bind,ro "$mirror"
mount --bind "$volume/git/example.test/o/r.git/worktrees/${admin##*/}" "$admin"
chmod 755 /work

w=/work/workspaces/a/r

echo "-- what a run pod sees"
equals "the volume's root has the mount points of git and workspaces, and nothing else" 'ls /work' 'git workspaces '
equals "no other run's directory" 'ls /work/workspaces' 'a '
no "no meta/" 'test -e /work/meta'
no "another run's files cannot be read" 'cat /work/workspaces/b/r/private.txt'
no "another mirror is not there" 'test -e /work/git/example.test/p'

echo "-- what git does in the run's worktree"
says "status names the branch" "cd $w && git status --short --branch" 'agent/a'
ok "log" "cd $w && git log --oneline | grep -q seed"
ok "describe finds the tag the mirror has" "cd $w && git describe --tags | grep -q v1.0"
ok "rev-parse" "cd $w && git rev-parse HEAD"
ok "diff, ls-files and grep" "cd $w && git diff --stat && git ls-files | grep -q README && git grep -q hi"
ok "worktree list" "cd $w && git worktree list"
ok "show a remote-tracking-free ref of the mirror" "cd $w && git show --stat main"
as_pod "cd $w && echo more >> README.md && touch new.txt"
says "status sees a change and an untracked file" "cd $w && git status --short" 'M README.md'
ok "the index is refreshed in the worktree's own directory" "cd $w && touch README.md && git status --short && test -f $admin/index"
ok "a file is put back from the index" "cd $w && git checkout -- README.md && ! git status --short | grep -q README"
ok "HEAD moves in the worktree's own directory" "cd $w && git switch --detach main && git switch agent/a"
ok "writes to the worktree and the run's directory" "echo y > $w/y.txt && echo y > /work/workspaces/a/y.txt && touch $admin/probe"
ok "a scratch project is a repository of its own and commits" "cd /work/workspaces/a/scratch && git add -A && git commit -qm s"

echo "-- what is refused"
no "add" "cd $w && echo z > z.txt && git add z.txt"
no "commit" "cd $w && git commit -qam x"
no "stash" "cd $w && echo z >> README.md && git stash"
no "reset --hard" "cd $w && git reset -q --hard"
no "tag" "cd $w && git tag evil"
no "branch" "cd $w && git branch evil"
no "config" "cd $w && git config --local user.name x"
no "a write to the mirror's config" "echo x >> $mirror/config"
no "a write to the mirror's hooks" "echo x > $mirror/hooks/post-checkout"
no "a new ref in the mirror" "mkdir $mirror/refs/heads/evil"
no "a new object in the mirror" "mkdir $mirror/objects/ff"
no "a new worktree directory in the mirror" "mkdir $mirror/worktrees/new"
no "a change to the mirror's HEAD" "echo ref: refs/heads/evil > $mirror/HEAD"

if [ "$fail" -ne 0 ]; then
  echo "git in a run pod FAILED"
  exit 1
fi
echo "git in a run pod passed"
