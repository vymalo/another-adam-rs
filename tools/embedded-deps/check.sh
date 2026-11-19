#!/usr/bin/env bash
# The embedded feature set (ADR 0047 decision 2), checked:
#
#   tools/embedded-deps/check.sh              every split crate builds with its defaults off and with each
#                                             feature alone, clippy -D warnings; then adam-engine's tree
#   tools/embedded-deps/check.sh tree         adam-engine's tree only
#   tools/embedded-deps/check.sh self-test    the tree check fails on a planted crate (axum, then rustls):
#                                             it adds each to adam-engine's manifest in this working tree,
#                                             expects a failure naming it, and puts the files back; and on
#                                             a gate turned back on (adam-ui/thread-tools, adam-a2a-runtime/push)
#
# The forbidden crates: an HTTP server or client, the A2A SDK's server and client, a database driver,
# the Kubernetes client, a TLS stack (the host has its own), the MCP client.
set -euo pipefail
cd "$(dirname "$0")/../.."

# `mio` is not in it: tokio's process support needs it, and the tools (slice 6) run commands.
FORBIDDEN='axum hyper h2 reqwest ureq curl curl-sys tower-http warp actix-web tonic tokio-tungstenite
a2a-server-lf a2a-client-lf utoipa-swagger-ui
sqlx sqlx-core tokio-postgres postgres mongodb rusqlite libsqlite3-sys diesel redis
kube k8s-openapi
rustls aws-lc-rs aws-lc-sys ring openssl openssl-sys native-tls
rmcp'

# crate:feature lists; each crate is also built with no feature at all.
SPLIT='adam-a2a:server adam-a2a-runtime:push adam-ui:thread-tools'

tree() {
  local package=$1 found=0 names
  # Every target, so a platform-only dependency (Windows, macOS) is seen from Linux too.
  names=$(cargo tree "${@:2}" -p "$package" -e normal --target all --prefix none | awk '{print $1}' | sort -u)
  for crate in $FORBIDDEN; do
    if grep -qx "$crate" <<<"$names"; then
      echo "embedded-deps: $package's tree holds $crate:" >&2
      cargo tree "${@:2}" -p "$package" -e normal --target all -i "$crate" >&2 || true
      found=1
    fi
  done
  [ "$found" = 0 ] && echo "embedded-deps: $package's tree holds none of the forbidden crates"
  return "$found"
}

# Each gate is on by default: a server build must not change, and the tests that need a gate
# (`required-features`) would be skipped, not failed, in a default build without it.
defaults() {
  local metadata
  metadata=$(cargo metadata --locked --no-deps --format-version 1)
  for entry in $SPLIT; do
    local crate=${entry%%:*} feature=${entry#*:}
    if ! jq -e --arg c "$crate" --arg f "$feature" \
      '.packages[] | select(.name == $c) | .features.default | index($f)' <<<"$metadata" >/dev/null; then
      echo "embedded-deps: $crate's default features do not include $feature" >&2
      return 1
    fi
  done
  echo "embedded-deps: every gate is on by default"
}

features() {
  defaults
  for entry in $SPLIT; do
    local crate=${entry%%:*} feature=${entry#*:}
    # Tests included: a test that needs the feature says so with `required-features`.
    echo "embedded-deps: $crate, no default feature"
    cargo clippy --locked -q -p "$crate" --no-default-features --all-targets -- -D warnings
    RUSTDOCFLAGS='-D warnings' cargo doc --locked -q -p "$crate" --no-deps --no-default-features
    # Not the doctests: the crate docs show the server, and CI runs every doctest with the defaults.
    cargo test --locked -q -p "$crate" --no-default-features --lib --tests
    echo "embedded-deps: $crate, only $feature"
    cargo clippy --locked -q -p "$crate" --no-default-features --features "$feature" --all-targets -- -D warnings
  done
  echo "embedded-deps: adam-engine"
  cargo clippy --locked -q -p adam-engine -- -D warnings
}

BACKUP=
restore() {
  [ -n "$BACKUP" ] || return 0
  cp "$BACKUP/Cargo.toml" crates/adam-engine/Cargo.toml
  cp "$BACKUP/Cargo.lock" Cargo.lock
  rm -rf "$BACKUP"
  BACKUP=
}

self_test() {
  local manifest=crates/adam-engine/Cargo.toml backup
  backup=$(mktemp -d)
  cp "$manifest" Cargo.lock "$backup/"
  # On any exit (a failed step under `set -e`, Ctrl-C), never leave a planted manifest behind.
  BACKUP=$backup
  trap restore EXIT INT TERM
  for planted in axum rustls; do
    # Under [dependencies], where a real one would be.
    awk -v dep="$planted.workspace = true" '{print} /^\[dependencies\]$/{print dep}' "$backup/Cargo.toml" >"$manifest"
    grep -qx "$planted.workspace = true" "$manifest" || { echo "embedded-deps self-test: could not plant $planted" >&2; return 1; }
    local out
    if out=$(tree adam-engine 2>&1); then
      echo "embedded-deps self-test: a planted $planted was not found" >&2
      return 1
    fi
    grep -q "holds $planted" <<<"$out" || { echo "embedded-deps self-test: the failure does not name $planted: $out" >&2; return 1; }
    echo "embedded-deps self-test: a planted $planted fails the check"
  done
  # A gate turned back on brings its crates back: the features are what keeps them out.
  cp "$backup/Cargo.toml" "$manifest"
  for gate in adam-ui/thread-tools:rmcp adam-a2a-runtime/push:axum; do
    local feature=${gate%%:*} crate=${gate#*:} out
    if out=$(tree adam-engine --features "$feature" 2>&1); then
      echo "embedded-deps self-test: $feature turned on was not found" >&2
      return 1
    fi
    grep -q "holds $crate" <<<"$out" || { echo "embedded-deps self-test: $feature did not bring $crate: $out" >&2; return 1; }
    echo "embedded-deps self-test: $feature turned on fails the check ($crate)"
  done
}

case "${1:-all}" in
  all) features; tree adam-engine --locked ;;
  tree) tree adam-engine --locked ;;
  self-test) self_test ;;
  *) echo "usage: $0 [all|tree|self-test]" >&2; exit 2 ;;
esac
