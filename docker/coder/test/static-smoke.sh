#!/bin/sh
# The seven binaries of the coder image are static and start in a foreign image.
#
#   static-smoke.sh <image>
#
# ADR 0053 decision 11: `adam-coder`, `adam-agent`, `adam-kube-exec`,
# `adam-egress`, `run-signer`, `adam-git-sandbox` and `adam-git` are built for
# x86_64-unknown-linux-musl and statically linked, so they start in any x86-64
# Linux image whatever its C library (ADR 0052 decision 10: the bench starts
# the coder from each task image). This script takes the binaries from the image
# and, for each, checks that `file` says statically linked (or static-pie
# linked), that the image itself defaults to uid 10001, and that each binary runs
# in debian:bookworm-slim and alpine:3.22 as uid 10001 with no configuration,
# ending on its own configuration error (78; 64 for adam-kube-exec) instead of a
# loader error. It also checks that each binary carries mimalloc, its global
# allocator. Needs docker, `file` and `grep`.
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <image>" >&2
  exit 2
fi
image=$1
work=$(mktemp -d)
container=""

cleanup() {
  status=$?
  if [ -n "$container" ]; then
    docker rm -f "$container" >/dev/null 2>&1 || true
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT INT TERM

# The binaries as the image ships them, not as a rebuild would make them.
container=$(docker create "$image")
for binary in adam-coder adam-agent adam-kube-exec adam-egress run-signer adam-git-sandbox adam-git; do
  docker cp "$container:/usr/local/bin/$binary" "$work/$binary" >/dev/null
  chmod 0755 "$work/$binary"
done
docker rm -f "$container" >/dev/null
container=""

# The exit code of a binary with no configuration at all, and the line it must say.
exit_code() {
  case $1 in
    adam-kube-exec) echo 64 ;;
    *) echo 78 ;;
  esac
}
message() {
  case $1 in
    adam-coder|adam-egress|run-signer|adam-git-sandbox) echo "invalid configuration" ;;
    adam-agent) echo "ADAM_AGENT_DIR is required" ;;
    adam-git) echo "GIT_SANDBOX_SOCKET" ;;
    adam-kube-exec) echo "expected run or shell" ;;
  esac
}

fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

# The image's own user: the deployment starts it without `--user`, so the runs
# below cannot prove the image's default by passing the flag themselves.
user=$(docker image inspect --format '{{.Config.User}}' "$image")
case $user in
  10001|10001:10001) ok "the image runs as uid 10001 (USER $user)" ;;
  *) bad "the image's default user is '${user:-root}', expected 10001" ;;
esac

for binary in adam-coder adam-agent adam-kube-exec adam-egress run-signer adam-git-sandbox adam-git; do
  out=$(file -b "$work/$binary")
  case $out in
    *"statically linked"*|*"static-pie linked"*) ok "$binary: $out" ;;
    *) bad "$binary is not static: $out" ;;
  esac
  # The global allocator is mimalloc (ADR 0053 decision 11). Nothing else in the tree depends on it,
  # so a binary without `#[global_allocator]` does not link its code.
  if grep -a -q "mimalloc" "$work/$binary"; then
    ok "$binary carries mimalloc"
  else
    bad "$binary does not carry mimalloc (its global allocator)"
  fi
done

# The index digests of the tags, so a moving tag cannot change what runs.
for base in \
  "debian:bookworm-slim@sha256:c67e92c8099ba22853ab4cfda7cfb494d253d6012ca82dee98ba8976772af018" \
  "alpine:3.22@sha256:9db2934886712d877c932e07249c75a59e6ca27adfbccc05261987a17bef2d63"; do
  docker pull -q "$base" >/dev/null
  for binary in adam-coder adam-agent adam-kube-exec adam-egress run-signer adam-git-sandbox adam-git; do
    want=$(exit_code "$binary")
    code=0
    # The uid is read inside the container: passing `--user` proves only what the
    # flag says, and a mutant that passes 0:0 still reaches the configuration check.
    # shellcheck disable=SC2016 # $0 and $(id -u) expand in the container's shell.
    out=$(timeout 20 docker run --rm --user 10001:10001 --network none \
      --mount "type=bind,src=$work/$binary,dst=/bin/$binary,readonly" \
      --entrypoint /bin/sh "$base" \
      -c 'uid=$(id -u) || exit 97; echo "uid=$uid"; test "$uid" = 10001 || exit 99; exec "/bin/$0"' \
      "$binary" 2>&1) || code=$?
    case $out in
      *"uid=10001"*) ;;
      *)
        bad "$binary in $base did not run as uid 10001: $out"
        continue
        ;;
    esac
    if [ "$code" -ne "$want" ]; then
      bad "$binary in $base exited $code, expected $want: $out"
      continue
    fi
    case $out in
      *"not found"*|*"No such file"*|*"error while loading"*|*"cannot execute"*)
        bad "$binary in $base: a loader error, not the configuration check: $out"
        continue
        ;;
    esac
    case $out in
      *"$(message "$binary")"*) ok "$binary in $base: exit $want, $(message "$binary")" ;;
      *) bad "$binary in $base exited $want but did not say '$(message "$binary")': $out" ;;
    esac
  done
done

if [ "$fail" -ne 0 ]; then
  exit 1
fi
echo "ok   the seven binaries are static and start in bookworm-slim and alpine"
