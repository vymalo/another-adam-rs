# The rootless Podman service

`adam-devcontainer` (ADR 0010) runs a run's commands in a devcontainer. The devcontainer CLI builds and
starts it through the API socket of a **rootless Podman service**, never through the host's Docker socket.
This directory is that service, for the local stack and for CI.

| File | What |
|---|---|
| `Containerfile` | Podman's own image, pinned by tag and digest, plus the user `agent` (uid 10001) |
| `seccomp.json` | the seccomp profile of containers/container-libs, pinned (below) |
| `../compose.devcontainer.yaml` | the override that adds the service to the stack and points the coder at it |

```sh
docker compose -f compose.yaml -f dev/compose.devcontainer.yaml --profile app up -d --build --wait
```

The base `compose.yaml` does not change: without the override the coder runs with
`DEVCONTAINER_RUNTIME=off`, in its own container.

## What the service container is given, and why

The service container has **no `privileged`, no `cap_add` and no `devices`**, and mounts no Docker socket.
It runs as uid 10001. Three `security_opt` entries are needed. They were *verified* on 2026-10-01 by the
planning of this change, by running nested containers in a Docker 29.6.2 container (kernel 6.18) from
`quay.io/podman/stable:v5.8.7-immutable`; they were **not re-run** while the change was written (no Podman in
that sandbox), and the CI job `devcontainer` of this repository is what repeats them:

| Option | Why |
|---|---|
| `seccomp=./dev/podman/seccomp.json` | Docker's default profile refuses `unshare`, `clone` with a new user namespace and `mount`, which a rootless Podman needs to make a container (`cannot clone`). The profile of containers/container-libs allows them (unconditionally, entry 1 of its `syscalls`) and is otherwise as strict as Docker's. It is **not** `seccomp=unconfined`. |
| `systempaths=unconfined` | Docker masks parts of `/proc` in every container (`maskedPaths`, `readonlyPaths`). A nested container mounts a fresh `/proc` of its own, and the kernel refuses that over a masked one (`crun: mount proc`). The upstream image works round it by binding the service's own `/proc` into nested containers, which would show a devcontainer every other run's processes and let it read the environment of any process with the same uid. This option costs little here: the service runs as uid 10001 with no effective capability. |
| `apparmor=unconfined` | The `docker-default` AppArmor profile has `deny mount,`. It does nothing on a host where AppArmor is off (the runs above), and is needed on one where it is on. |

Not needed, and so not given (*verified* by the same runs): `--privileged`, `cap_add: SYS_ADMIN`, `/dev/fuse`
(storage is native overlay on the volume) and `/dev/net/tun` (nested containers share the service's network
namespace instead of `pasta`). `containers.conf` of the image sets the namespaces of nested containers to
`host` (that is, the service container's own) and `cgroups="disabled"`; so there are **no per-container
limits** (the service's own `cpus`, `mem_limit` and `pids_limit` bound all of them), and a container does
not survive a restart of the service (the coder finds that out and rebuilds, with a step that says so).

**Ubuntu 24.04 hosts** (the CI runners, many desktops) set
`kernel.apparmor_restrict_unprivileged_userns=1`, which stops a rootless Podman from making a user namespace
(*unverified*: a secondary source, marcioapm/lux#35; the CI job is the proof). Set it to `0`:

```sh
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
```

Without it the service starts but cannot run a container, and the coder goes on in its own environment with a
step that says the runtime is not reachable. It degrades; it does not break.

## `seccomp.json`: where it comes from

* **Source:** `common/pkg/seccomp/seccomp.json` of <https://github.com/containers/container-libs> (the
  successor of the archived containers/common) at the tag `common/v0.67.2`, which is what
  `go.podman.io/common` is at in the `go.mod` of Podman v5.8.7 (*verified* 2026-10-01).
* **Commit:** `62c711d302a4349b959816d5f3d1b5d8b3a7df73` (the commit the tag points to).
* **sha256:** `2598b3b98e6970f37f917e210202fa8976aefcd99abf8955803a6e35bba17eb4`, 17705 bytes.

```sh
curl -fsSL -o dev/podman/seccomp.json \
  https://raw.githubusercontent.com/containers/container-libs/62c711d302a4349b959816d5f3d1b5d8b3a7df73/common/pkg/seccomp/seccomp.json
echo '2598b3b98e6970f37f917e210202fa8976aefcd99abf8955803a6e35bba17eb4  dev/podman/seccomp.json' | sha256sum -c -
```

The planning run that found these options used a copy of the profile whose source was not recorded
(sha256 `886ae167…`). It allows exactly the same syscalls as this one; this one adds rules that refuse
`socket` for some address families. The integration test of CI is what shows that the pinned file is enough.
Bump it with the image: the tag of `go.podman.io/common` in Podman's `go.mod` says which one goes with which.

*Unverified:* that Compose resolves the relative path `seccomp=./dev/podman/seccomp.json` against the
project directory (`docker compose config` accepts it). If a Compose version does not, set
`PODMAN_SECCOMP=$PWD/dev/podman/seccomp.json`.

## The integration test

`crates/adam-devcontainer/tests/podman.rs` builds a fixture repository (`devbox`: a Dockerfile that adds a tool
only its devcontainer has), runs commands in it (stdin, stdout, the exit code), checks that a file made inside
is the coder's outside, that git reads the read-only mirror, that `kill` stops what a command left running,
that the container's environment has no credential, that a name of the service's network resolves, and that
`release` leaves no container, no image and no file the coder cannot delete. It needs the service to share a
directory with the test at the **same path**, and the test must run as the service's uid:

```sh
work=/tmp/adam-devcontainer; sock=/tmp/adam-podman
sudo install -d -o 10001 -g 10001 "$work" "$sock"
PODMAN_SOCKET_DIR=$sock PODMAN_WORK=$work PODMAN_WORK_TARGET=$work \
  docker compose -f compose.yaml -f dev/compose.devcontainer.yaml --profile app up -d --build --wait podman
cargo test -p adam-devcontainer --test podman --no-run
sudo -E setpriv --reuid 10001 --regid 10001 --clear-groups env PATH="$PATH" \
  ADAM_TEST_DEVCONTAINER=1 ADAM_TEST_REQUIRE_DEVCONTAINER=1 \
  CONTAINER_HOST=unix://$sock/podman.sock ADAM_TEST_DEVCONTAINER_ROOT=$work \
  ADAM_TEST_DEVCONTAINER_DNS_NAME=podman \
  <the test binary cargo printed, copied where uid 10001 can run it> --nocapture
```

It needs the devcontainer CLI (`npm install -g @devcontainers/cli@0.89.0`) and Podman's client on `PATH`.
`.github/workflows/ci.yml`, the job `devcontainer`, does exactly this.
