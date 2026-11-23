#!/usr/bin/env bash
# A host that installs its own TLS provider builds adam-model-openai and adam-model-oauth with
# `--no-default-features`: neither may then link aws-lc-rs. `self-test` plants the feature and expects a failure.
set -euo pipefail
cd "$(dirname "$0")/../.."

PACKAGES='adam-model-openai adam-model-oauth'

check() {
  local found=0 out
  for p in $PACKAGES; do
    cargo build -p "$p" --locked --no-default-features >/dev/null
    out=$(cargo tree -p "$p" --locked --no-default-features -e features -i aws-lc-rs 2>&1 || true)
    if [[ "$out" == *"did not match any packages"* ]]; then
      echo "tls-provider: $p holds no aws-lc-rs without its defaults"
    else
      echo "tls-provider: $p links aws-lc-rs without its defaults:" >&2
      echo "$out" | head -20 >&2
      found=1
    fi
  done
  return "$found"
}

case "${1:-check}" in
  check) check ;;
  self-test)
    cp crates/adam-model-openai/Cargo.toml /tmp/openai.Cargo.toml.bak
    trap 'cp /tmp/openai.Cargo.toml.bak crates/adam-model-openai/Cargo.toml' EXIT
    sed -i 's/^default = \["aws-lc"\]/default = []/; s/^\[dependencies\]/[dependencies]\nreqwest = { workspace = true, features = ["rustls"] }/' crates/adam-model-openai/Cargo.toml
    sed -i '0,/^reqwest.workspace = true/{/^reqwest.workspace = true/d}' crates/adam-model-openai/Cargo.toml
    if check >/dev/null 2>&1; then echo "tls-provider: self-test: the check passed with aws-lc-rs planted" >&2; exit 1; fi
    echo "tls-provider: self-test: the check fails when aws-lc-rs returns" ;;
  *) echo "usage: $0 [check|self-test]" >&2; exit 2 ;;
esac
