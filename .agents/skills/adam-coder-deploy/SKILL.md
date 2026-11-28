---
name: adam-coder-deploy
description: "Build, publish and deploy the adam-rs coder image (ghcr.io/vymalo/another-adam-rs/coder, which also carries adam-agent) and its Helm chart deploy/coder: image tags sha-<7> for development and releases X.Y.Z (signed images and the OCI chart, cut by release-please), the CI that builds and bumps image.tag, chart topologies and placements, secrets, render checks. Use when changing docker/coder or deploy/coder, releasing the image, or deploying the coder or a folder agent to Kubernetes."
---

# Build, publish and deploy the coder image and chart

One image, `ghcr.io/vymalo/another-adam-rs/coder`, carries seven static binaries: `adam-coder` (the
entrypoint), `adam-agent` (run with `--entrypoint tini ... -- adam-agent`, see `adam-agent-folder`),
`adam-kube-exec`, `adam-egress`, `run-signer` (the loopback sidecar that signs a run's model requests with
the run's own certificate, ADR 0086), `adam-git-sandbox` and `adam-git` (run pods and the git sandbox, see
`deploy/coder/README.md`). One Helm chart, `deploy/coder`, deploys the coder. There is no second image.
A release also publishes the chart as an OCI package, `oci://ghcr.io/vymalo/another-adam-rs/charts/coder`.

On Kubernetes the coder (or a folder agent) can also run as `AgentService` and `AgentConfig` custom
resources under the operator of vymalo/another-agentic-platform instead of this chart
(https://github.com/vymalo/another-agentic-platform/blob/main/deploy/operator/README.md). Pick one per agent, never both
(the chart's objects block the operator's).

Every adam-rs path below is in `vymalo/another-adam-rs` at the revision you pin (replace `main`
by that revision). Entry point:
https://github.com/vymalo/another-adam-rs/blob/main/deploy/coder/README.md.

## When to use

* Changing `docker/coder/Dockerfile`, the base image, a pinned tool, or the chart.
* Releasing a version (`vX.Y.Z`), releasing a new development image, or finding which image tag a
  commit produced.
* Deploying the coder (or an `adam-agent` folder agent) on Kubernetes, or in compose.
* Not for choosing which adam-rs revision a consumer pins: `adam-upgrade`.
* Not for running agents as custom resources: the operator is in vymalo/another-agentic-platform, not here.

## Procedure

1. **The image.** `docker/coder/Dockerfile` builds from the **repository root**
   (`docker buildx build -f docker/coder/Dockerfile .`): stage 1 compiles the seven binaries for
   `x86_64-unknown-linux-musl` on a pinned `rust:1.98-trixie` (tag and digest, plus `musl-tools` and the
   target), statically linked with `mimalloc` as the global allocator, so they start in any x86-64 Linux
   image whatever its C library (ADR 0053 decision 11); stage 2 is the workspace image
   `ghcr.io/vymalo/another-agentic-images/workspace:${WORKSPACE_TAG}` (the `ARG WORKSPACE_TAG` is
   an immutable `<rust toolchain>-<sha7>` tag; it moves together with `runPods.image` of the chart) with the
   binaries and the pinned `github-mcp-server` added. The image carries no devcontainer CLI and no container
   engine's client (ADR 0052). Runtime user is
   uid 10001, `/work` is the workspace volume, `EXPOSE 8080`, entrypoint
   `tini -- adam-coder`. The build argument `ADAM_BUILD_REVISION` (CI passes the commit) is baked into the
   binaries and shows on the card as the version's `+<sha7>` and in `build/v1` (ADR 0028). Tool pins carry a "verified <date>" comment: move a tag, its digest and
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
   `image.tag` by hand in a pull request. This is development: `sha-<7>` images are unsigned.
4. **Releases (ADR 0084).** release-please (`.github/workflows/release-please.yml`,
   `release-please-config.json`, `.release-please-manifest.json`) keeps one release pull request,
   `chore: release main`, open on `main`; merging it tags `vX.Y.Z`. One version for the repository:
   every line marked `# x-release-please-version` (the workspace `version`, every path dependency's
   `version`, `deploy/coder/Chart.yaml`'s `version` and `appVersion`) moves together, and the job
   then runs `tools/release/sync.sh` on the release branch (`cargo metadata` for the lockfiles,
   `deploy/coder/tests/render-goldens.sh` for the goldens, whose labels and ConfigMap checksums carry
   the chart's version). On the tag, `.github/workflows/release.yml` waits for `coder.yml`'s image
   job to pass on that commit, builds the image again with `provenance: mode=max` and `sbom: true`,
   pushes it by digest, smoke-tests that digest, signs it with cosign keyless and only then tags it
   `X.Y.Z` and `X.Y` (never `latest`); it packages `deploy/coder` with `image.tag` set to `X.Y.Z` in a
   copy (`bump-tag.sh` accepts `X.Y.Z`), pushes it to `oci://ghcr.io/vymalo/another-adam-rs/charts`
   and signs it. A new path dependency with a version needs the mark, or
   `sh tools/release/check-versions.sh` (CI, job `fmt`) fails. The release needs a GitHub App
   (secrets `RELEASE_PLEASE_APP_CLIENT_ID`, `RELEASE_PLEASE_APP_PRIVATE_KEY`): a tag made with
   `GITHUB_TOKEN` starts no workflow. Every pull request title is a Conventional Commit
   (`tools/commit-lint.sh`, workflow `commit-lint.yml`): it is the commit release-please reads.
   **Production deploys a release**, pinned `X.Y.Z@sha256:<digest>`:

   ```sh
   helm install coder oci://ghcr.io/vymalo/another-adam-rs/charts/coder --version X.Y.Z \
     -n <ns> -f my-values.yaml --set image.tag=X.Y.Z@sha256:<digest>
   cosign verify ghcr.io/vymalo/another-adam-rs/coder:X.Y.Z \
     --certificate-identity "https://github.com/vymalo/another-adam-rs/.github/workflows/release.yml@refs/tags/vX.Y.Z" \
     --certificate-oidc-issuer https://token.actions.githubusercontent.com
   ```
5. **The chart** (`deploy/coder/README.md` documents every value):
   * `topology: combined` (default, one StatefulSet) or `split` (a front Deployment with
     `ROLE=control-plane` plus the worker StatefulSet).
   * `database.enabled: true` (default) creates a CloudNativePG `Cluster`; `database.enabled: false`
     with `database.existingSecret.name` (and `.key`, `uri` by default) reads `DATABASE_URL` of every
     pod from a Secret you provide and renders no `Cluster`. The role must be able to create tables
     in its default schema: every role migrates at startup (`deploy/coder/README.md`, "Database").
     The server must be **PostgreSQL 18 or later** (ADR 0082; an older one is exit 78 before any DDL):
     the `Cluster` pins `database.imageName` at 18.7 by tag and digest, and an existing `Cluster` on 17
     takes an offline major upgrade on the release that brings the pin (back it up first; keep its
     distribution: `deploy/coder/README.md`, "PostgreSQL 18").
   * `workspace.placement`: empty, `isolated`, `affinity` or `shared` (ADR 0002,
     `docs/decisions/0002-workspace-placement.md`); `replicaCount` above 1 needs one.
   * `github.auth: token` (default) or `app` (`github.app.id`, `.privateKeySecret`, and exactly
     one of `.installationId`, a pin to one installation, or `.owners`, the accounts the App may
     act for, the installation of each found with the App's key: `GITHUB_APP_OWNERS`, ADR 0017;
     there is no default list, because a public App can be installed by anyone);
     `createRepoOwners` turns the repository-creation tool on (an App that names its accounts does it by itself, not
     with `owners: ["*"]`). `accessTiers.max` (`MAX_ACCESS_TIER`, empty is `auto`) caps the access tiers a person may pick;
     `bypass` is opt-in. An App's key is readable by the commands of a run in the coder's own container, so the coder refuses to
     start that way (exit 78) and the chart refuses to render `github.auth: app` with `runPods.enabled: false`, unless
     `runPods.allowCommandsNextToSecrets: true` (`ALLOW_COMMANDS_NEXT_TO_SECRETS=true`) accepts the risk: use run pods
     (`deploy/coder/examples/production.values.yaml`).
   * `githubMcp` (default on): the official GitHub MCP server as a **native sidecar** (an init
     container with `restartPolicy: Always`, Kubernetes 1.29+) of every pod that runs workers,
     `github-mcp-server http --read-only` on 127.0.0.1:8082 from the coder's own image. It holds no
     Secret, key or token: the coder sends the credentials of each call, and `GITHUB_MCP_URL` says
     where it is. The coder's shipped agent files name port 8082 (ADR 0017).
   * Secrets come from an `ExternalSecret` (`externalSecrets.*`): `MODEL_API_KEY`, `GITHUB_TOKEN`,
     `A2A_BEARER_TOKENS`. The defaults point at one owner's cluster (`secretStoreRef`, `key`):
     override them for yours.
   * `activity.enabled` (off by default and then invisible): `GET /internal/activity` on a port of its own
     (`activity.port`, 8081; `ACTIVITY_LISTEN_ADDR`) with tokens of its own (`ACTIVITY_BEARER_TOKENS` from
     `activity.tokensSecret.name`/`.key`, a Secret you manage, never a value), on the pods the Service selects, a
     Service port `activity`, and a NetworkPolicy rule that gives `activity.allowedNamespace` that port alone. The A2A
     port then stops serving the route; without it the route stays on the A2A port behind the A2A tokens
     (**deprecated**). An activity token that is also an A2A token, or the address without tokens, is exit 78 (ADR 0083,
     `docs/decisions/0083-the-activity-route-has-a-listener-and-tokens-of-its-own.md`; chart README, "The activity port").
     The chart's render is read by another-agentic-platform's parity goldens: say so in the pull request.
   * The model gateway's URL (`MODEL_BASE_URL`) is the literal `config.modelBaseUrl` (a placeholder by
     default). To keep it out of git set `config.modelBaseUrlFromSecret: true`: the workers read it by
     `secretKeyRef` and the ExternalSecret copies `externalSecrets.properties.modelBaseUrl`
     (`model_base_url`) from `externalSecrets.key`. Leave `config.modelBaseUrl` unset (the placeholder
     counts as unset; a real URL beside the option is refused, as are `externalSecrets.enabled=false`, a
     missing property name and `MODEL_BASE_URL` in `config.extraEnv`). Add the AWS property before turning
     it on: a missing property fails the whole ExternalSecret sync. Chart only: the binary reads the
     env var either way, so there is no image dependency (chart README, "The model gateway's URL from the
     secret").
   * `config.modelAuth` (`MODEL_AUTH`, ADR 0067): empty (default, the binary's `key`), `key`, `device` (the
     device grant: on the workers, `MODEL_OAUTH_ISSUER` and `MODEL_OAUTH_CLIENT_ID` are required in
     `config.extraEnv`, and the chart refuses `device` without them; `MODEL_OAUTH_SCOPE` is optional) or
     `injector`: no key, every model call carries `X-Run-Id` and no `Authorization`, an injector in front of the
     model adds the run's credential, and, on the workers, any `MODEL_OAUTH_*` beside it is refused. With
     `device` or `injector` the ExternalSecret still creates `MODEL_API_KEY`, so the AWS value of
     `externalSecrets.properties.modelApiKey` must be EMPTY (a non-empty one is exit 78; the chart warns in
     `NOTES.txt`, it cannot read the value). With `injector`, point `config.modelBaseUrl` at the injector or every
     run parks up to `MODEL_PARK_LIMIT` (`NOTES.txt` warns when the placeholder is still there). Workers only.
     `MODEL_CA_FILE` has no chart value yet (ADR 0067 decision 9, a later slice). Chart only, no image dependency
     (chart README, "Model authentication").
   * `config.modelContextWindow` (null): `DEFAULT_MODEL_CONTEXT_WINDOW` (ADR 0088; it rendered `MODEL_CONTEXT_WINDOW`,
     now the binary's deprecated alias, before 2026-11-28), the model's window in tokens when its profile names no
     `contextWindow`, said by each usage report, and the output cap's window: the cap is clamped to the window less
     20,000 and the coder's history (ADR 0064). Refused beside either name in `config.extraEnv`; the binary exits 78
     when both names are set to different values. **Image dependency**: an image built before the rename reads only
     the old name, so the new chart with an older `image.tag` reports no window. The render changed for this value,
     which another-agentic-platform's parity goldens read: say so in the pull request (`deploy/coder/README.md`,
     "The model's context window").
   * `mcp.websearch.url` (our web search MCP Service, in cluster) and `mcp.context7.enabled` (hosted
     Context7), both off by default and byte-for-byte invisible when off. With either on, the chart renders
     ONE file of servers (a ConfigMap in the shape of `mcp.json`, no copy of the agent's prompt) and sets
     `ADAM_EXTRA_MCP_FILE` on the workers: the binary adds those servers to its own `mcp.json` at startup
     (a name clash is exit 78). Their keys (`SEARCH_MCP_TOKEN`, `CONTEXT7_API_KEY`) come from the
     ExternalSecret, never from values; both servers are `optional` by default (down, or no key: skipped
     with a warning, not exit 69); a plain `http` URL needs `mcp.websearch.allowInsecure` (or
     `config.extraEnv.MCP_ALLOW_INSECURE`). Put the AWS properties in place before turning one on (chart
     README, "Extra MCP servers").
   * `cache.allowedClaims` (empty by default): the chart mounts **no cache**. The image keeps the caches of npm,
     pnpm and cargo under `/cache`, a directory of each container's own file system; a deployment that wants them
     across pods gives each run its own claim through the platform
     (`AgentConfig.environment.cache.existingClaim`), and this value names the claims a **run pod** may mount besides
     the work claim — the admission policy refuses any other. A claim is one trust domain, its root must be writable
     by uid and gid 10001, and cargo takes no lock on NFS (ADR 0040, amended 2026-11-26,
     `docs/decisions/0040-a-shared-package-cache-at-cache.md`; chart README, "The package cache"). The image must be
     on the workspace base `1.98.1-8c297d3` or later, which has the caches under `/cache`.
   * No Ingress: the service is cluster-internal and reached over A2A with a bearer token.
   * A repository's own environment is a run pod of its image, never a container engine in the coder
     (ADR 0052). The chart's answer is `runPods.enabled` (off by default and then invisible): each active run's processes run in a pod of its own
     (`RUN_ENVIRONMENT=kubernetes`, ADR 0019, `docs/decisions/0019-a-runs-processes-in-a-pod-of-their-own.md`). The
     chart renders the pod template (a ConfigMap; image, resources and volumes are values), a `PriorityClass` and a
     `ResourceQuota` scoped to it (a quota cannot select pods by label), a ServiceAccount with a Role for `pods` and
     `pods/exec`, a `ValidatingAdmissionPolicy` that refuses every pod of that ServiceAccount but the intended one,
     and a `NetworkPolicy`; the coder pod gets a 1Gi limit (`runPods.coderResources`). It needs the chart's floor,
     Kubernetes 1.37+ (ADR 0080). A run pod reads no Secret, so it needs no ExternalSecret (ADR 0038); the chart
     refuses `replicaCount` above 1 on per-pod `ReadWriteOnce` volumes and a policy
     switched off without `runPods.admissionPolicy.disableAcknowledged`. The image ships `adam-kube-exec` and
     `/opt/adam/bin/adam-exec` for the pod's init container (no `opencode`, ADR 0038) (chart README, "Run pods").
     A repository may bring its own `devcontainer.json`: with `runPods.repositoryImages.allowedPrefixes` (empty by
     default) the coder offers `use_repository_environment` and the policy admits an image under a prefix for the `run`
     container alone (ADR 0052, `docs/decisions/0052-no-podman-a-repositorys-own-environment-is-a-run-pod-of-its-image.md`).
   * The deployment's hook programs are **not** a chart value yet (ADR 0050, slice P3): the binaries
     already read `ADAM_HOOKS_FILE` (a mounted JSON file of `programs`) and `HOOKS_ALLOW_PROGRAMS=true`,
     start the programs that apply to the agent they serve, and exit 78 when one cannot start. Until
     the chart renders a `hooks.file`, mount the file and its program outside the agent's folder and
     workspace and set both variables by hand (`docs/reference/environment.md`).
6. **Change the chart with its guards.** Render-time validation is in
   `deploy/coder/templates/_validate.tpl`; the default render is pinned by
   `deploy/coder/tests/golden/combined.yaml`; `deploy/coder/tests/render-check.sh` asserts the
   guarantees on the render. A new CRD kind needs its schema in `deploy/coder/tests/schemas`
   (kubeconform has no `-ignore-missing-schemas`). The chart is **Helm 4 only** (v4.3.2 in CI, ADR 0080) and its
   `kubeVersion` floor is **Kubernetes 1.37**: `helm template --kube-version 1.36.0` fails. Helm 4 installs with
   server-side apply, `--wait` is the kstatus watcher (`--wait=legacy` for the old behaviour) and `--atomic` is
   deprecated for `--rollback-on-failure`.
7. **Deploy a folder agent from the same image**: run the image with the entrypoint
   `tini -- adam-agent`, the folder mounted at `ADAM_AGENT_DIR`, readable by uid 10001
   (`adam-agent-folder`). The chart deploys the coder only. For a folder agent use an `AgentService`
   (the platform's operator, https://github.com/vymalo/another-agentic-platform/blob/main/deploy/operator/README.md) or write your own manifest, using the chart's StatefulSet as a model for the variables.
8. **Local stack**: `docker compose --profile app up -d --build --wait` (see `compose.yaml` and
   `docs/guides/run-locally.md` and `docs/reference/dev-stack.md`).

## Verify

```sh
helm lint deploy/coder
helm template coder deploy/coder --namespace <ns> > /dev/null
sh deploy/coder/tests/render-check.sh        # from the repository root
sh deploy/coder/tests/bump-tag-test.sh
sh tools/release/check-versions.sh           # every version a release moves is marked, and the Cargo lines agree
sh deploy/coder/tests/render-goldens.sh && git diff --exit-code deploy/coder/tests/golden   # goldens are the chart's render
shellcheck deploy/coder/bump-tag.sh deploy/coder/tests/*.sh docker/coder/test/*.sh dev/*.sh
docker buildx build -f docker/coder/Dockerfile -t coder:smoke --load .
sh docker/coder/test/static-smoke.sh coder:smoke    # `file` says static; each starts in debian:bookworm-slim and alpine (run-signer also on loopback, stopped by SIGTERM)
sh docker/coder/test/container-smoke.sh coder:smoke   # needs a reachable Postgres, see the script header; EXPECT_REVISION=<sha> checks the build argument
sh docker/coder/test/agent-smoke.sh coder:smoke
```

The smoke tests also check that `/docs` and `/openapi.json` are public and that `GET /tasks` (the HTTP+JSON binding) needs
the token (`docker/coder/test/http-smoke.sh`,
https://github.com/vymalo/another-adam-rs/blob/main/docker/coder/test/http-smoke.sh).

**Swagger UI on the cluster.** The chart has no Ingress and adds no route to the docs; reach them with a port-forward
to the Service (the release `coder` in `another-agentic-system` here):

```sh
kubectl -n another-agentic-system port-forward svc/coder 8080:8080
# open http://127.0.0.1:8080/docs, press Authorize, paste a token of A2A_BEARER_TOKENS, then Try it out
```

Try it out calls the address the page was opened from, so it works through the port-forward. `config.extraEnv.A2A_DOCS:
"false"` turns the docs off (chart README, "Swagger UI",
https://github.com/vymalo/another-adam-rs/blob/main/deploy/coder/README.md#swagger-ui).

Then the compose scenarios of the `image` job in `.github/workflows/coder.yml`
(`dev/coder-e2e.sh`, `dev/greeting-e2e.sh`, `dev/agent-e2e.sh`, `dev/coder-choices-e2e.sh`,
`dev/agent-cards-e2e.sh`): read the job for the exact order and variables.

## Pitfalls

* A new GHCR package is private until its owner makes it public: check an anonymous pull before
  a deployment points at it (`adam-upgrade` has the token and manifest request).
* The build context is the repository root, not `docker/coder`.
* The binaries are static (musl), but their TLS roots are not: `adam-coder`, `adam-agent` and
  `adam-kube-exec` read the system's CA bundle, so an image that runs the coder without CA certificates cannot
  reach an HTTPS gateway, GitHub or a git remote.
* A musl build needs `musl-gcc` in the builder (`musl-tools`); a missing one fails in `aws-lc-sys` or
  `mimalloc`'s C sources, not in Rust code.
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
* Do not push `image.tag` yourself: a human bump races the workflow's. Never set it to a release on
  `main`: the release sets `X.Y.Z` in the packaged copy only.
* Do not edit a version line by hand, or the release pull request: release-please owns them. A
  release changes the chart's render (two labels per object and the ConfigMap checksums), and the
  release pull request carries the re-rendered goldens.
* A release's image digest differs from its commit's `sha-<7>`: it is rebuilt, with provenance and an
  SBOM. Renaming `release.yml` breaks every `cosign verify` of past releases (the identity is the file).
* The chart's OCI package is private until its owner makes it public, like any new GHCR package.

## See also

* `deploy/coder/README.md`, `bin/adam-coder/README.md`, `bin/adam-agent/README.md`
  ("Image and compose"), `docs/guides/deploy-the-coder.md`.
* `adam-agent-folder`, `adam-upgrade`.
* https://github.com/vymalo/another-adam-rs/blob/main/.github/workflows/coder.yml
* https://github.com/vymalo/another-adam-rs/blob/main/.github/workflows/release.yml and
  https://github.com/vymalo/another-adam-rs/blob/main/docs/guides/releases.md
