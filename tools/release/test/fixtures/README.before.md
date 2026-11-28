# Vymalo Homebrew tap

Install the [another-agentic desktop app](https://github.com/vymalo/another-agentic-system) with [Homebrew](https://brew.sh/).
The tap provides a macOS Cask and a Linux AppImage Formula, each for arm64 and x86_64. Windows is not distributed.

## Install and launch

Install Homebrew first using the instructions at [brew.sh](https://brew.sh/). Then choose your platform:

### macOS — Apple Silicon or Intel

```sh
brew install --cask vymalo/tap/another-agentic
open -a another-agentic
```

### Linux — arm64 or x86_64

```sh
brew install --formula vymalo/tap/another-agentic
another-agentic
```

Homebrew selects the download for your architecture. You can also add the tap explicitly with `brew tap vymalo/tap` before installing; keep the fully qualified names above to select this tap and the correct package type.
The GitHub repository is named `homebrew-tap`, but its Homebrew name is `vymalo/tap` ([Homebrew naming convention](https://docs.brew.sh/Taps)).

The app connects to the deployment it was built for; it does not start a local server. See the [app documentation](https://github.com/vymalo/another-agentic-system/blob/main/apps/tauri/README.md) for deployment and sign-in details.

## Upgrade

```sh
brew update
```

Then upgrade only this app:

```sh
# macOS
brew upgrade --cask vymalo/tap/another-agentic

# Linux
brew upgrade --formula vymalo/tap/another-agentic
```

The tap's recipes determine the available version. Downloads come from [GitHub Releases](https://github.com/vymalo/another-agentic-system/releases) and are checked against the recipes' SHA-256 checksums.

## Uninstall

Run the command for your platform:

```sh
# macOS
brew uninstall --cask vymalo/tap/another-agentic

# Linux
brew uninstall --formula vymalo/tap/another-agentic
```

These commands do not explicitly erase the app's saved profile. After removing all packages installed from this tap, you can remove the tap itself:

```sh
brew untap vymalo/tap
```

## Launch troubleshooting

- **macOS:** Current bundles are unsigned. A successful Homebrew install does not prove that Gatekeeper permits launch. Follow [Apple's guidance for safely opening apps](https://support.apple.com/en-us/102445) and your device's security policy; do not disable Gatekeeper or remove quarantine as a general workaround.
- **Linux:** Launch requires a graphical session, compatible system libraries, and working AppImage runtime support. For FUSE errors, see the [official AppImage troubleshooting guide](https://docs.appimage.org/user-guide/troubleshooting/fuse.html). Its extract-and-run option can be used without FUSE:

  ```sh
  another-agentic --appimage-extract-and-run
  ```

Installation checks are not proof of GUI launch or successful sign-in. Report app issues in [another-agentic-system](https://github.com/vymalo/another-agentic-system/issues), including your OS, architecture, and error message.

Usage commands and platform caveats verified against the current tap recipes and the linked Homebrew, Apple, and AppImage documentation on 2026-10-12.
