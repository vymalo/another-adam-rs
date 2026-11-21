# The coder image

`ghcr.io/vymalo/another-adam-rs/coder` is built from this directory, from the **repository root**:

```sh
docker buildx build -f docker/coder/Dockerfile .
```

Stage 1 compiles six binaries for `x86_64-unknown-linux-musl` on the pinned `rust:1.94-trixie` (with
`musl-tools`), so each is **statically linked** and starts in any x86-64 Linux image whatever its C library
([ADR 0053](../../docs/decisions/0053-sqlite-a-store-service-a-database-per-agent-and-a-static-coder.md)
decision 11). Every one sets `mimalloc` as its global allocator (`#[global_allocator]`), because musl's own
is slow under many threads:

| Binary | What it is |
|---|---|
| `adam-coder` | the entrypoint: the coder agent over A2A |
| `adam-agent` | any agent folder over A2A (`--entrypoint tini -- adam-agent`) |
| `adam-kube-exec` | runs one command in a run pod, for the coder |
| `adam-egress` | the HTTPS-only egress proxy of the run pods |
| `adam-git-sandbox` | the server that runs the coder's git where no secret is readable |
| `adam-git` | the relay that stands in for git and speaks to that server |

Stage 2 is the workspace image
(`ghcr.io/vymalo/another-agentic-images/workspace:${WORKSPACE_TAG}`) with the binaries in
`/usr/local/bin` and the pinned `github-mcp-server` (no devcontainer CLI and no container engine's client: ADR 0052). It runs as
uid 10001, keeps its files in `/work` and its entrypoint is `tini -- adam-coder`. The base stays a glibc
image because the tools in it need one; the binaries do not.

Static linking brings no certificates: the binaries that speak HTTPS (`adam-coder`, `adam-agent`,
`adam-kube-exec`) read the system's CA bundle, so an image that runs them without one cannot reach an HTTPS
gateway, GitHub or a git remote.

## Tests

| Script | What it checks |
|---|---|
| `test/static-smoke.sh <image>` | `file` says each binary is static, the image's default user is 10001, and each starts in `debian:bookworm-slim` and Alpine as uid 10001, ending on its configuration error (78; 64 for `adam-kube-exec`), never a loader error |
| `test/container-smoke.sh <image>` | the coder starts under tini, serves A2A fail-closed and takes a task to completion against a stub model (needs a Postgres) |
| `test/agent-smoke.sh <image>` | the same image with `--entrypoint tini -- adam-agent` |
| `test/http-smoke.sh <base-url> <bearer-token>` | the HTTP+JSON binding, `/docs` and `/openapi.json` (run by the container and agent smoke tests) |
| `test/github-mcp-tools.sh` | the GitHub MCP server lists the coder's read tools over stdio, with no credential |

`.github/workflows/coder.yml` runs the build, `static-smoke.sh`, the other smoke tests and the compose
scenarios. Deployment is in [`deploy/coder/README.md`](../../deploy/coder/README.md); the walk-through is
[`docs/guides/deploy-the-coder.md`](../../docs/guides/deploy-the-coder.md).
