#!/bin/sh
# Installs adam-agent through Homebrew from a formula built out of a published release's checksums, and runs it.
#
#   homebrew-smoke.sh <version> <SHA256SUMS> <Cargo.toml>
#
# The formula is the one the tap would get (write-homebrew-formula.mjs), put in a throwaway local tap: Homebrew
# installs only from taps. It downloads the archive of this machine from the release, checks its SHA-256, runs
# the formula's own `test do` (`adam-agent --version`) and then the installed binary. Needs brew and node.
set -eu

if [ "$#" -ne 3 ]; then
  echo "usage: $0 <version> <SHA256SUMS> <Cargo.toml>" >&2
  exit 2
fi
version=$1
sums=$2
cargo_toml=$3
here=$(cd "$(dirname "$0")" && pwd)

# The runner images that ship Homebrew do not all put it on PATH.
if ! command -v brew >/dev/null 2>&1; then
  for prefix in /home/linuxbrew/.linuxbrew /opt/homebrew /usr/local; do
    if [ -x "$prefix/bin/brew" ]; then
      eval "$("$prefix/bin/brew" shellenv)"
      break
    fi
  done
fi
command -v brew >/dev/null 2>&1 || { echo "no brew on this runner" >&2; exit 1; }

export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_FROM_API=1 HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_ENV_HINTS=1
tap=local/adam-smoke
brew tap-new --no-git "$tap" >/dev/null
dir="$(brew --repository "$tap")"
node "$here/write-homebrew-formula.mjs" --no-readme "$dir" "$version" "$sums" "$cargo_toml"

brew install "$tap/adam-agent"
brew test "$tap/adam-agent"
out=$(adam-agent --version)
echo "$out"
case "$out" in
  "adam-agent $version"*) ;;
  *) echo "adam-agent --version says '$out', want 'adam-agent $version...'" >&2; exit 1 ;;
esac
# Startup, not just the version: no configuration is a configuration error (78), not a crash or a hang.
code=0
env -i PATH="$PATH" adam-agent >/dev/null 2>&1 || code=$?
if [ "$code" -ne 78 ]; then
  echo "adam-agent with no configuration exited $code, want 78" >&2
  exit 1
fi
brew uninstall "$tap/adam-agent"
brew untap "$tap" >/dev/null 2>&1 || true
