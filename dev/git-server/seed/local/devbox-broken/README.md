# devbox-broken

A repository whose `.devcontainer/devcontainer.json` asks for what a devcontainer may not have
(`privileged`), tries to run a command on the coder's side (`initializeCommand`) and to read the
coder's token (`${localEnv:GITHUB_TOKEN}`). The coder refuses the file, says so, and asks the person
how to go on; nothing in the file runs.
