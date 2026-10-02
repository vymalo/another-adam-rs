# devbox

A tiny repository whose work environment is its own devcontainer (`.devcontainer/`): its
Dockerfile puts `devbox-tool` in the image, and nothing else has it. The coder runs its
commands, its checks and OpenCode in that container, so `devbox-tool --version` prints
`devbox-tool 1.0 (from the devcontainer)` there, and `command not found` anywhere else.

The repository's check is `sh check.sh`: it passes only where `devbox-tool` is the devcontainer's, and,
if `tool.txt` exists, only if it says so.
