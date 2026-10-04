---
name: adam-coder-deploy
description: "Build, publish and deploy the adam-rs coder image (ghcr.io/vymalo/another-adam-rs/coder, which also carries adam-agent) and its Helm chart deploy/coder: image tags sha-<7>, the CI that builds and bumps image.tag, chart topologies and placements, secrets, render checks. Use when changing docker/coder or deploy/coder, releasing the image, or deploying the coder or a folder agent to Kubernetes."
---

# Build, publish and deploy the coder image and chart

One image, `ghcr.io/vymalo/another-adam-rs/coder`, carries two binaries: `adam-coder` (the
entrypoint) and `adam-agent` (run with `--entrypoint tini ... -- adam-agent`, see
`adam-agent-folder`). One Helm chart, `deploy/coder`, deploys the coder. There is no second image
or package.

Every adam-rs path below is in `vymalo/another-adam-rs` at the revision you pin (replace `main`
by that revision). Entry point:
https://github.com/vymalo/another-adam-rs/blob/main/deploy/coder/README.md.

## When to use

* Changing `docker/coder/Dockerfile`, the base image, a pinned tool, or the chart.
* Releasing a new image, or finding which image tag a commit produced.
* Deploying the coder (or an `adam-agent` folder agent) on Kubernetes, or in compose.
* Not for choosing which adam-rs revision a consumer pins: `adam-upgrade`.

## Procedure

1. **The image.** `docker/coder/Dockerfile` builds from the **repository root**
   (`docker buildx build -f docker/coder/Dockerfile .`): stage 1 compiles `adam-coder` and
   `adam-agent` on a pinned `rust:1.94-trixie` (tag and digest); stage 2 is the workspace image
   `ghcr.io/vymalo/another-agentic-images/workspace:${WORKSPACE_TAG}` (the `ARG WORKSPACE_TAG` is
   an immutable `<rust toolchain>-<sha7>` tag) with the binaries, the pinned
   `github-mcp-server`, the devcontainer CLI and Podman's remote client added. Runtime user is
   uid 10001, `/work` is the workspace volume, `EXPOSE 8080`, entrypoint
   `tini -- adam-coder`. Tool pins carry a "verified <date>" comment: move a tag, its digest and
   the claims next to it together (the GitHub MCP server's are also in
   `docs/decisions/0009-github-per-installation-read-through-mcp.md` and in
   `.github/workflows/ci.yml`, job `conformance`).
2. **The tag.** Images are tagged `sha-<first 7 of the commit>`. The `coder` workflow
   (`.github/workflows/coder.yml`) builds the image on every pull request that touches the
   paths it lists, smoke-tests it, runs the compose scenarios, and on `main` pushes **the very
   image that passed** (`docker tag coder:smoke "$IMAGE:$tag"`). The image carries the label
   `org.opencontainers.image.revision` with the full commit sha.
3. **The chart tag follows by itself.** After a push the workflow's `bump` job runs
   `deploy/coder/bump-tag.sh deploy/coder/values.yaml sha-<7>` and pushes
   `chore(deploy): bump coder to sha-<7>` to main (it is idempotent, edits only `image.tag`, and
   skips when `main` moved on). Pull requests get a dry run that shows the edit. Do not edit
   `image.tag` by hand in a pull request.
4. **The chart** (`deploy/coder/README.md` documents every value):
   * `topology: combined` (default, one StatefulSet) or `split` (a front Deployment with
     `ROLE=control-plane` plus the worker StatefulSet).
   * `workspace.placement`: empty, `isolated`, `affinity` or `shared` (ADR 0002,
     `docs/decisions/0002-workspace-placement.md`); `replicaCount` above 1 needs one.
   * `github.auth: token` (default) or `app` (`github.app.id`, `.privateKeySecret`, and exactly
     one of `.installationId`, a pin to one installation, or `.owners`, the accounts the App may
     act for, the installation of each found with the App's key: `GITHUB_APP_OWNERS`, ADR 0017;
     there is no default list, because a public App can be installed by anyone);
     `createRepoOwners` turns the repository-creation tool on.
   * `githubMcp` (default on): the official GitHub MCP server as a **native sidecar** (an init
     container with `restartPolicy: Always`, Kubernetes 1.29+) of every pod that runs workers,
     `github-mcp-server http --read-only` on 127.0.0.1:8082 from the coder's own image. It holds no
     Secret, key or token: the coder sends the credentials of each call, and `GITHUB_MCP_URL` says
     where it is. The coder's shipped agent files name port 8082 (ADR 0017).
   * Secrets come from an `ExternalSecret` (`externalSecrets.*`): `MODEL_API_KEY`, `GITHUB_TOKEN`,
     `A2A_BEARER_TOKENS`. The defaults point at one owner's cluster (`secretStoreRef`, `key`):
     override them for yours.
   * `mcp.websearch.url` (our web search MCP Service, in cluster) and `mcp.context7.enabled` (hosted
     Context7), both off by default and byte-for-byte invisible when off. With either on, the chart renders
     ONE file of servers (a ConfigMap in the shape of `mcp.json`, no copy of the agent's prompt) and sets
     `ADAM_EXTRA_MCP_FILE` on the workers: the binary adds those servers to its own `mcp.json` at startup
     (a name clash is exit 78). Their keys (`SEARCH_MCP_TOKEN`, `CONTEXT7_API_KEY`) come from the
     ExternalSecret, never from values; both servers are `optional` by default (down, or no key: skipped
     with a warning, not exit 69); a plain `http` URL needs `mcp.websearch.allowInsecure` (or
     `config.extraEnv.MCP_ALLOW_INSECURE`). Put the AWS properties in place before turning one on (chart
     README, "Extra MCP servers").
   * No Ingress: the service is cluster-internal and reached over A2A with a bearer token.
   * Devcontainers are off on Kubernetes (README "Devcontainers are off here").
5. **Change the chart with its guards.** Render-time validation is in
   `deploy/coder/templates/_validate.tpl`; the default render is pinned by
   `deploy/coder/tests/golden/combined.yaml`; `deploy/coder/tests/render-check.sh` asserts the
   guarantees on the render. A new CRD kind needs its schema in `deploy/coder/tests/schemas`
   (kubeconform has no `-ignore-missing-schemas`).
6. **Deploy a folder agent from the same image**: run the image with the entrypoint
   `tini -- adam-agent`, the folder mounted at `ADAM_AGENT_DIR`, readable by uid 10001
   (`adam-agent-folder`). The chart deploys the coder only; for a folder agent write your own
   manifest, using the chart's StatefulSet as a model for the variables.
7. **Local stack**: `docker compose --profile app up -d --build --wait` (see `compose.yaml` and
   "Local development" in the root `README.md`).

## Verify

```sh
helm lint deploy/coder
helm template coder deploy/coder --namespace <ns> > /dev/null
sh deploy/coder/tests/render-check.sh        # from the repository root
sh deploy/coder/tests/bump-tag-test.sh
shellcheck deploy/coder/bump-tag.sh deploy/coder/tests/*.sh docker/coder/test/*.sh dev/*.sh
docker buildx build -f docker/coder/Dockerfile -t coder:smoke --load .
sh docker/coder/test/container-smoke.sh coder:smoke   # needs a reachable Postgres, see the script header
sh docker/coder/test/agent-smoke.sh coder:smoke
```

Then the compose scenarios of the `image` job in `.github/workflows/coder.yml`
(`dev/coder-e2e.sh`, `dev/greeting-e2e.sh`, `dev/agent-e2e.sh`, `dev/coder-choices-e2e.sh`,
`dev/agent-cards-e2e.sh`): read the job for the exact order and variables.

## Pitfalls

* A new GHCR package is private until its owner makes it public: check an anonymous pull before
  a deployment points at it (`adam-upgrade` has the token and manifest request).
* The build context is the repository root, not `docker/coder`.
* A workspace base tag that is a placeholder (`...PENDING`) makes the workflow skip the build
  with a warning instead of failing.
* The image sets no `MCP_ALLOW_STDIO`; `adam-agent` from this image refuses local-process MCP
  servers unless its own deployment sets it. The coder no longer needs it: its shipped agent files
  read the GitHub MCP server over http (the sidecar), so a coder started without the sidecar stops
  at startup with exit 69. The chart still sets `MCP_ALLOW_STDIO` for one release, for agent
  folders written before the sidecar.
* In compose, run the GitHub MCP server beside the coder in its network namespace
  (`network_mode: service:coder`, the same image, entrypoint `tini -- github-mcp-server`, command
  `http --read-only --toolsets context,repos,issues,pull_requests --listen-host 127.0.0.1 --port
  8082`), as `compose.yaml`'s `github-mcp` does.
* `volumeClaimTemplates` are immutable: switching `workspace.placement` between a per-pod volume
  and a shared one needs `kubectl delete statefulset <release>-coder --cascade=orphan` before
  `helm upgrade` (chart README, "Workspace placement").
* `topology: split` moves the Service selector to the front: upgrade at a quiet moment.
* Do not push `image.tag` yourself: a human bump races the workflow's.

## See also

* `deploy/coder/README.md`, `bin/adam-coder/README.md`, `bin/adam-agent/README.md`
  ("Image and compose"), `docs/architecture.md` ("How it is deployed").
* `adam-agent-folder`, `adam-upgrade`.
* https://github.com/vymalo/another-adam-rs/blob/main/.github/workflows/coder.yml
