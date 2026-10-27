#!/bin/sh
# What git needs in a run pod (ADR 0041), with real git and real mounts, no cluster.
#
# A run pod mounts its own run's directory read-write, a worktree's mirror READ-ONLY and the worktree's own
# directory in the mirror (<mirror>/worktrees/<name>) read-write on top, each at the path the coder has it at, and
# nothing else of the volume. On top of the worktree's directory, READ-ONLY, are four entries the coder's git
# follows out of it or acts on: HEAD, commondir and gitdir (single-file mounts) and logs (a directory). This builds
# that view in a private mount namespace (bind mounts, as the container runtime makes them from the pod's subPath
# mounts) and runs git as uid 10001 in it:
#
#   * what a repository's checks do with git works: status (also after the working tree changed: the index is
#     refreshed in the worktree's own directory), log, diff, describe, rev-parse, grep;
#   * what writes an object, a ref or the configuration of the shared mirror fails: add, commit, stash, reset, tag,
#     branch, config, and a write to the mirror's config, hooks, refs, objects or worktrees/; so does moving HEAD
#     (switch, checkout of another commit), which appends to the reflog in logs/;
#   * nothing of another run, of meta/ or of another mirror is there, and a scratch project (its own repository
#     inside the run's directory) commits;
#   * the four read-only entries cannot be written, appended to, renamed over, replaced by a link or removed (the
#     coder's git would follow a rewritten commondir to another repository, appends to a reflog that is a link,
#     and resets the branch a rewritten HEAD names, which could be another run's), the files owned by the very uid
#     that tries, and git still works with them: nothing git does for what a check needs writes HEAD;
#   * every slot (the worktree and the scratch project) is a mount point of its own on top of the run's directory:
#     the pod cannot rename it, move it, remove it or exchange it with a sibling, which is what lets the coder's
#     git, which finds its work tree by name, rely on the name (EBUSY); a directory of the run that is not a slot
#     still renames, is replaced and is exchanged (the control: the refusals are the mounts' doing), the slot keeps
#     its inode, and git works in it.
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
exchange=$(mktemp)
mkdir -p /work
trap 'umount -R /work 2>/dev/null || true; rmdir /work 2>/dev/null || true; rm -rf "$volume" "$exchange"' EXIT
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
mkdir /work/workspaces/a/sibling /work/workspaces/a/sibling2
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

# --- the run pod's view of run a: a tmpfs for the image's /work and the mounts, parents first
umount /work
mount -t tmpfs tmpfs /work
mkdir -p /work/workspaces/a "$mirror"
mount --bind "$volume/workspaces/a" /work/workspaces/a
# What the slot mounts below close: with only the run's directory mounted, the pod renames a slot and puts what it likes
# at its name (this is the pod as it was before: `mv` and back leaves the volume as it was).
ok "without a mount of its own a slot can be renamed and replaced by a link (what the slot mounts close)" \
  "mv /work/workspaces/a/scratch /work/workspaces/a/scratch.moved && ln -s /work/workspaces/b/r /work/workspaces/a/scratch && rm /work/workspaces/a/scratch && mv /work/workspaces/a/scratch.moved /work/workspaces/a/scratch"
# Each slot is a mount of its own on top of the run's directory (the pod's subPath mounts of workspaces/a/r and
# workspaces/a/scratch): a mount point cannot be renamed, moved, removed or replaced from inside the pod.
mount --bind "$volume/workspaces/a/r" /work/workspaces/a/r
mount --bind "$volume/workspaces/a/scratch" /work/workspaces/a/scratch
mount --bind "$volume/git/example.test/o/r.git" "$mirror"
mount -o remount,bind,ro "$mirror"
mount --bind "$volume/git/example.test/o/r.git/worktrees/${admin##*/}" "$admin"
# The entries of the worktree's directory the coder's git follows or acts on: read-only on top of it.
for entry in HEAD commondir gitdir logs; do
  mount --bind "$volume/git/example.test/o/r.git/worktrees/${admin##*/}/$entry" "$admin/$entry"
  mount -o remount,bind,ro "$admin/$entry"
done
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
ok "blame, show, for-each-ref and the branch's name" "cd $w && git blame README.md >/dev/null && git show -s HEAD >/dev/null && git for-each-ref >/dev/null && test \"\$(git branch --show-current)\" = agent/a"
ok "show a remote-tracking-free ref of the mirror" "cd $w && git show --stat main"
as_pod "cd $w && echo more >> README.md && touch new.txt"
says "status sees a change and an untracked file" "cd $w && git status --short" 'M README.md'
ok "the index is refreshed in the worktree's own directory" "cd $w && touch README.md && git status --short && test -f $admin/index"
ok "a file is put back from the index" "cd $w && git checkout -- README.md && ! git status --short | grep -q README"
no "HEAD cannot be moved by git: its reflog (logs/HEAD) is read-only, and git appends to it" "cd $w && git switch --detach main"
says "and git says why" "cd $w && git switch --detach main" 'Read-only file system'
ok "writes to the worktree and the run's directory" "echo y > $w/y.txt && echo y > /work/workspaces/a/y.txt && touch $admin/probe"
ok "a scratch project is a repository of its own and commits" "cd /work/workspaces/a/scratch && git add -A && git commit -qm s"

echo "-- what the pod cannot change in the worktree's directory"
no "HEAD cannot be written (it would name another run's branch, which the coder would then reset)" "echo 'ref: refs/heads/agent/b' > $admin/HEAD"
no "HEAD cannot be appended to" "echo x >> $admin/HEAD"
no "HEAD cannot be renamed over" "echo 'ref: refs/heads/agent/b' > $admin/h2 && mv $admin/h2 $admin/HEAD"
no "HEAD cannot be removed" "rm $admin/HEAD"
no "HEAD cannot be replaced by a link" "ln -sf /etc/passwd $admin/HEAD"
no "git cannot point HEAD at another branch" "cd $w && git symbolic-ref HEAD refs/heads/agent/b"
no "git cannot detach HEAD (update-ref --no-deref)" "cd $w && git update-ref --no-deref HEAD main"
equals "HEAD says what git wrote" "cat $admin/HEAD" 'ref: refs/heads/agent/a '
no "commondir cannot be written" "echo /elsewhere > $admin/commondir"
no "commondir cannot be appended to" "echo x >> $admin/commondir"
no "commondir cannot be renamed over" "echo /elsewhere > $admin/c2 && mv $admin/c2 $admin/commondir"
no "commondir cannot be removed" "rm $admin/commondir"
no "commondir cannot be replaced by a link" "ln -sf /etc/passwd $admin/commondir"
no "gitdir cannot be written" "echo /work/workspaces/b/r/.git > $admin/gitdir"
no "gitdir cannot be renamed over" "echo x > $admin/g2 && mv $admin/g2 $admin/gitdir"
no "gitdir cannot be removed" "rm $admin/gitdir"
no "logs cannot be moved away" "mv $admin/logs $admin/logs.moved"
no "logs cannot be removed" "rm -r $admin/logs"
no "a link cannot be put at logs/HEAD" "rm $admin/logs/HEAD && ln -s /etc/passwd $admin/logs/HEAD"
no "logs/HEAD cannot be replaced" "echo x > $admin/logs/new && mv $admin/logs/new $admin/logs/HEAD"
no "nothing can be made in logs" "touch $admin/logs/new"
equals "commondir says what git wrote" "cat $admin/commondir" '../.. '
equals "and so does the volume" "cat /work/git/example.test/o/r.git/worktrees/${admin##*/}/commondir" '../.. '
if [ "$(cat "$volume/git/example.test/o/r.git/worktrees/${admin##*/}/HEAD")" = "ref: refs/heads/agent/a" ] && [ "$(cat "$volume/git/example.test/o/r.git/worktrees/${admin##*/}/commondir")" = "../.." ] && [ -f "$volume/git/example.test/o/r.git/worktrees/${admin##*/}/logs/HEAD" ] && [ ! -L "$volume/git/example.test/o/r.git/worktrees/${admin##*/}/logs/HEAD" ]; then
  echo "ok   the coder's side of the volume is as git wrote it"
else
  echo "FAIL the coder's side of the volume changed"; fail=1
fi
ok "git still works with them in place" "cd $w && git status --short && git log --oneline | grep -q seed && git diff --stat"
ok "the other entries of the directory are writable (the index, a lock, a probe)" "touch $admin/probe2 && cd $w && touch README.md && git status --short && test -f $admin/index"

echo "-- what the pod cannot do to a slot's directory"
slots="/work/workspaces/a/r /work/workspaces/a/scratch"
inodes=$(as_pod "stat -c '%i' $slots" | tr '\n' ' ')
# renameat2(RENAME_EXCHANGE) of two paths, by hand: coreutils' mv has no --exchange on the runner. Prints the error.
cat > "$exchange" <<'PY'
import ctypes, os, sys
libc = ctypes.CDLL(None, use_errno=True)
AT_FDCWD, RENAME_EXCHANGE = -100, 2
if libc.renameat2(AT_FDCWD, os.fsencode(sys.argv[1]), AT_FDCWD, os.fsencode(sys.argv[2]), RENAME_EXCHANGE) != 0:
    err = ctypes.get_errno()
    print(os.strerror(err))
    sys.exit(err)
PY
chmod 644 "$exchange"
if command -v python3 >/dev/null; then
  have_exchange=1
elif [ "${ADAM_TEST_REQUIRE_MOUNTS:-}" = 1 ]; then
  echo "FAIL python3 is needed for the exchange of two directories, and ADAM_TEST_REQUIRE_MOUNTS=1"; fail=1; have_exchange=0
else
  echo "skipped the exchange of two directories: no python3"; have_exchange=0
fi
for slot in $slots; do
  name=${slot##*/}
  no "the slot $name cannot be renamed" "mv $slot $slot.moved"
  says "and the reason is that it is a mount point ($name)" "mv $slot $slot.moved" 'busy'
  no "the slot $name cannot be moved to another name of the run's directory" "mv $slot /work/workspaces/a/elsewhere"
  no "the slot $name cannot be removed" "rmdir $slot"
  says "and the reason is that it is a mount point (rmdir of $name)" "rmdir $slot" 'busy'
  no "the slot $name cannot be replaced by a link to another run's slot" "mv $slot $slot.moved && ln -s /work/workspaces/b/r $slot"
  if [ "$have_exchange" -eq 1 ]; then
    no "the slot $name cannot be exchanged with a sibling" "python3 $exchange $slot /work/workspaces/a/sibling"
    says "and the reason is that it is a mount point (exchange of $name)" "python3 $exchange $slot /work/workspaces/a/sibling" 'busy'
  fi
done
no "the run's own directory cannot be renamed either" "mv /work/workspaces/a /work/workspaces/a2"
equals "the slots are the very directories they were" "stat -c '%i' $slots" "$inodes"
ok "the slots are what they were: a worktree's .git file, a scratch project's .git directory" "test -f /work/workspaces/a/r/.git && test -d /work/workspaces/a/scratch/.git"
echo "-- the control: what is not a slot is the pod's to rename, replace and exchange"
ok "a directory of the run that is not a slot renames" "mv /work/workspaces/a/sibling /work/workspaces/a/sibling.moved && mv /work/workspaces/a/sibling.moved /work/workspaces/a/sibling"
ok "and can be replaced by a link" "mv /work/workspaces/a/sibling /work/workspaces/a/sibling.moved && ln -s /work/workspaces/b /work/workspaces/a/sibling && rm /work/workspaces/a/sibling && mv /work/workspaces/a/sibling.moved /work/workspaces/a/sibling"
if [ "$have_exchange" -eq 1 ]; then
  ok "and exchanged with another" "python3 $exchange /work/workspaces/a/sibling /work/workspaces/a/sibling2 && python3 $exchange /work/workspaces/a/sibling /work/workspaces/a/sibling2"
fi
ok "git works in the slots that are mounts" "cd $w && git status --short --branch | grep -q agent/a && cd /work/workspaces/a/scratch && git status --short"
ok "and the slot is written in, as before" "echo z > $w/z.txt && echo z > /work/workspaces/a/scratch/z.txt && rm $w/z.txt /work/workspaces/a/scratch/z.txt"

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
