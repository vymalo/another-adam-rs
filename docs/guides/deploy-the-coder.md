# Deploy the coder

The coder is one image, `ghcr.io/vymalo/another-adam-rs/coder` (built from `docker/coder/Dockerfile`), deployed
by the Helm chart in [`deploy/coder`](../../deploy/coder/README.md). The chart README documents every value;
this page is the path through the decisions. How the chart reaches the cluster after merge is outside this
repository (*unverified* here).

## What you get

One container, one process, one database. Reachable **only inside the cluster**: the orchestrator calls A2A on
the Service with a bearer token; there is no Ingress.

```mermaid
flowchart LR
    orch["Orchestrator<br/>namespace another-agentic-system"]
    gw["OpenAI-compatible<br/>model gateway"]
    remote["Git remote<br/>github.com"]
    ghapi["GitHub REST API"]

    subgraph k8s["Kubernetes: Helm chart deploy/coder"]
        np["NetworkPolicy<br/>ingress only from the orchestrator namespace"]
        svc["Service<br/>ClusterIP :8080, no Ingress"]
        subgraph pod["Pod: StatefulSet, 1 replica, uid 10001"]
            tini["tini (PID 1)<br/>forwards SIGTERM, reaps children"]
            coder["adam-coder<br/>A2A server + workers"]
            kids["sh, git<br/>child processes"]
            tini --> coder
            coder --> kids
        end
        pvc[("PVC at /work<br/>mirrors, workspaces, notes")]
        cnpg[("CloudNativePG cluster<br/>Postgres: runs and journal")]
        secret["ExternalSecret to Secret<br/>MODEL_API_KEY, GITHUB_TOKEN (not for role control-plane, nor with github.auth=app), A2A_BEARER_TOKENS"]
    end

    orch -->|"A2A JSON-RPC + bearer token"| svc
    np -.->|guards| svc
    svc --> coder
    coder --- pvc
    coder -->|"DATABASE_URL"| cnpg
    secret -.-> coder
    coder -->|"chat completions"| gw
    coder -->|"git fetch, git push"| remote
    coder -->|"find and open pull request"| ghapi
```

`topology: split` runs the two halves as separate workloads: a stateless front (`ROLE=control-plane`, no volume)
the Service selects, and the worker StatefulSet (`ROLE=worker`, `/healthz` only):

```mermaid
flowchart LR
    orch["Orchestrator<br/>namespace another-agentic-system"]
    gw["OpenAI-compatible<br/>model gateway"]
    remote["Git remote and GitHub REST API"]

    subgraph k8s["Kubernetes: Helm chart deploy/coder, topology split"]
        svc["Service<br/>ClusterIP :8080, selects the front"]
        front["Deployment: front, front.replicas<br/>ROLE=control-plane, no volume"]
        worker["StatefulSet: worker, 1 replica<br/>ROLE=worker, /healthz only"]
        pvc[("PVC at /work")]
        cnpg[("CloudNativePG cluster<br/>Postgres: runs and journal")]
    end

    orch -->|"A2A JSON-RPC + bearer token"| svc
    svc --> front
    front -->|"DATABASE_URL"| cnpg
    worker -->|"DATABASE_URL"| cnpg
    worker --- pvc
    worker -->|"chat completions"| gw
    worker -->|"git, pull requests"| remote
```

## The image

`docker/coder/Dockerfile`, built from the **repository root**, has three stages: it compiles `adam-coder`,
`adam-agent`, `adam-kube-exec`, `adam-egress`, `run-signer` and the two binaries of `adam-git-sandbox` for
`x86_64-unknown-linux-musl` on a pinned `rust:1.98-trixie` (tag and digest, plus `musl-tools` for the C
sources), so each is statically linked with `mimalloc` as its allocator and starts in any x86-64 Linux image
whatever its C library ([ADR 0053](../decisions/0053-sqlite-a-store-service-a-database-per-agent-and-a-static-coder.md)
decision 11; the image's smoke test runs each in `debian:bookworm-slim` and Alpine, as uid 10001, and expects
its configuration error, never a loader error — and `run-signer`, which serves, is also started on loopback and
stopped with SIGTERM there). `adam-egress` is the HTTPS-only proxy for run pods, [`bin/adam-egress`](../../bin/adam-egress/README.md),
smoke-tested in the image and run by the chart with run pods; `run-signer` is the loopback sidecar that signs a run's
model requests with the run's own certificate, [`bin/run-signer`](../../bin/run-signer/README.md), smoke-tested in the
image (the deployment that runs it beside an agent is a later change); `adam-git-sandbox` is the server that runs the coder's git
where no secret is readable and `adam-git` the relay that stands in for git, [`crates/adam-git-sandbox`](../../crates/adam-git-sandbox/README.md),
smoke-tested in the image and run by the chart, with run pods, as a container of the coder's pod that mounts the same work volume and holds no secret;
a deployment without run pods that sets `GIT_SANDBOX_SOCKET` runs the server itself); it takes the official `github-mcp-server` binary (pinned by tag and digest); and the runtime is the
`workspace` image of `vymalo/another-agentic-images` (Rust, Flutter/Dart, Node, git, tini, the obscura
headless browser), pinned by an immutable tag, with those binaries added. It runs as uid 10001, listens on `0.0.0.0:8080`, keeps its files in `/work`, and its entrypoint is
`tini -- adam-coder` (`tini` forwards SIGTERM and reaps children). Tags are `sha-<7>` of the commit. The binaries are smoke-tested in CI
(`docker/coder/test/`). Static linking brings no certificates: the binaries that speak HTTPS (`adam-coder`,
`adam-agent`, `adam-kube-exec`) still read the system's CA bundle for HTTPS (the `workspace` base has one), so
an image that runs the coder without one cannot reach the model gateway, GitHub or a git remote.

The served card lists `child-sessions/v1` (`explorer` and `reviewer`, which a person can open as sessions; `maxOpen` is `SESSION_MAX_OPEN`). The chart does not change; with a split deployment, roll the workers before the control plane.

The build argument `ADAM_BUILD_REVISION` (CI passes the commit) is baked into the binaries. The agent card then says
which build answers: `version` is `<crate version>+<first 7>` and the `build/v1` extension carries the whole revision
and a digest of the agent's files ([ADR 0028](../decisions/0028-the-card-says-which-build-answers.md), contract in
[A2A server](../reference/a2a-server.md)). A build without the argument says `unknown`. The smoke tests check the card
against `EXPECT_REVISION`, which CI sets to the commit.

## Before you start

| You need | Why |
|---|---|
| Helm 4.3.2 or later | the chart is Helm 4 only ([ADR 0080](../decisions/0080-helm-4-and-kubernetes-1-37-a-hard-cutover.md)); `helm install` uses server-side apply |
| Kubernetes 1.37+ | the chart's floor ([ADR 0080](../decisions/0080-helm-4-and-kubernetes-1-37-a-hard-cutover.md)); the GitHub MCP server and, with run pods, the git sandbox are native sidecars, and run pods use a `ValidatingAdmissionPolicy` |
| the CloudNativePG operator, **or** an existing Postgres | the run store; `database.enabled: false` + `database.existingSecret.name` for your own |
| External Secrets (`ExternalSecret`, `external-secrets.io/v1`) | the three secrets below; the defaults point at one owner's `secretStoreRef` and `key`, override them |
| a StorageClass | the PVC at `/work` (a ReadWriteMany one for `shared` or `affinity` placement) |
| an OpenAI-compatible model gateway | `config.modelBaseUrl`, with its `/v1` prefix; or an injector in front of it (`config.modelAuth: injector`, ADR 0067) |
| a GitHub token, or a GitHub App | to push and open pull requests |

## Secrets

Nothing secret is in values or in git.

| Variable | AWS property (default) | For |
|---|---|---|
| `MODEL_API_KEY` | `adam_coder_model_api_key` | roles that run workers; its value is empty with `config.modelAuth: device` or `injector` |
| `GITHUB_TOKEN` | `adam_coder_github_token` | workers, with `github.auth: token` (the default) |
| `A2A_BEARER_TOKENS` | `adam_coder_a2a_bearer_tokens` (comma-separated) | roles that serve A2A (the front, with `split`) |
| `MODEL_BASE_URL` | `model_base_url` | only with `config.modelBaseUrlFromSecret: true`, to keep the URL out of git |

Put an AWS property in place **before** turning an option on that reads it: a missing property fails the whole
`ExternalSecret` sync. With a GitHub App there is no `GITHUB_TOKEN`; you make the Secret that holds the App's
private key (`private-key.pem`) yourself and name it in `github.app.privateKeySecret`; the chart never holds it.
The orchestrator holds one of the bearer tokens.

## Install

```sh
helm lint deploy/coder
helm template coder deploy/coder --namespace another-agentic-system          # read what it will create
sh deploy/coder/tests/render-check.sh                                          # the guarantees, asserted on the render
helm upgrade --install coder deploy/coder --namespace another-agentic-system -f my-values.yaml
```

`image.tag` is bumped by CI on every green build of `main` (`sha-<7>`); do not edit it by hand.

**Upgrading a release installed with chart 0.4.x or earlier** needs one extra step before this: `podManagementPolicy` is
immutable, so delete the StatefulSet once, keeping its pods (`kubectl delete statefulset <release>-coder
--cascade=orphan`). See [the chart README](../../deploy/coder/README.md#workspace-placement).

The values you will actually set:

| Value | Decision |
|---|---|
| `config.modelBaseUrl`, `config.model` | the gateway and the model alias (`coder`). `config.opencodeModel` is gone with OpenCode (ADR 0038): left in a values file it changes nothing, and `NOTES.txt` says it is ignored |
| `config.modelAuth` | how the workers authenticate: empty (default, the binary's `key`), `key`, `device` (signs in with the device grant; on the roles that run workers, `MODEL_OAUTH_ISSUER` and `MODEL_OAUTH_CLIENT_ID` are required in `config.extraEnv`, and the chart refuses `device` without them) or `injector` (ADR 0067: no key, every call names its run with `X-Run-Id`; on the workers, any `MODEL_OAUTH_*` is refused, and the base URL must point at the injector or runs park up to `MODEL_PARK_LIMIT`; set the AWS value of `externalSecrets.properties.modelApiKey` to empty). [The chart](../../deploy/coder/README.md#model-authentication) |
| `config.publicUrl` | the URL clients reach (goes into the agent card); empty is the in-cluster Service URL |
| `topology` | `combined` (default) or `split`. Switch with `--set topology=split`; the Service selector moves to the front once it exists, so do it at a quiet moment |
| `replicaCount`, `workspace.placement` | more than one worker **needs** a placement (`isolated`, `affinity` or `shared`, [why](../reference/workspace-and-environments.md#placement-which-worker-holds-a-runs-files)); the chart refuses `replicaCount > 1` without one. Switching between a per-pod and a shared volume needs `kubectl delete statefulset <release>-coder --cascade=orphan` first |
| `github.auth`, `github.app.*` | `token`, or `app` with exactly one of `installationId` (pinned) or `owners` (the accounts it may act for; no default list). **An App needs `runPods.enabled: true`**, or `runPods.allowCommandsNextToSecrets: true` to accept the risk: [upgrading](#upgrading-an-app-deployment) |
| `config.allowedRepoHosts`, `config.githubApiUrl` | repository hosts the token may go to; GitHub Enterprise: add the host and `https://<host>/api/v3` |
| `github.createRepoOwners` | owners the coder may create repositories for (a list restricts); empty: the accounts a GitHub App names (not `owners: ["*"]`), and nothing for a token. The person agrees first, unless they picked the `bypass` access tier |
| `accessTiers.max` | the highest access tier a person may pick: `plan`, `edits`, `auto` (empty, the default) or `bypass`, which is opt-in (rename and delete repositories). With an App and the commands in the coder's container, a command can read the key: [the access tiers](../../deploy/coder/README.md#the-access-tiers) |
| `database.*` | CloudNativePG `Cluster` (default) or `existingSecret` |
| `mcp.websearch.url`, `mcp.context7.enabled` | optional extra MCP servers, off by default |
| `a2a.push.allowedUrls`, `a2a.cardSigning.secretName` | optional A2A push notifications (the webhooks they may reach) and a signed agent card (a Secret with the key), both off by default ([the chart](../../deploy/coder/README.md#push-notifications-and-the-cards-signature-a2a)) |
| `config.modelExtraBody`, `config.modelEchoReasoning` | to make a model emit its reasoning |
| `config.maxParallelToolCalls` | how many tool calls of one model turn run at the same time, 1 to 32 (`MAX_PARALLEL_TOOL_CALLS`; unset: 10) |
| `config.maxProcesses`, `config.maxGitProcesses` | the processes of a worker at once, 1 to 256 (`MAX_PROCESSES`: commands, background commands and language servers; unset: 4), and its git processes, 1 to 64 (`MAX_GIT_PROCESSES`; unset: 2) |
| `config.commandMemoryMax`, `config.commandCpuMax` | the memory (`2GB`) and the CPU (`500m`, `2`) one command may use (`COMMAND_MEMORY_MAX`, `COMMAND_CPU_MAX`; unset: a quarter of the RAM at most 8 GB, half the cores) |
| `config.modelContextWindow` | the model's context window in tokens (`MODEL_CONTEXT_WINDOW`), so a screen shows how full the context is, and the output cap's window: each call asks no more than the window less 20,000 and the history the agent keeps (ADR 0064; [the chart](../../deploy/coder/README.md#the-models-context-window)) |
| `config.modelProfiles` | the per-model profiles (ADR 0063, ADR 0064): per model id, the reasoning levels it takes and how they are sent, its output cap, its extra body and its preset, as one JSON file (`MODEL_ALIASES_FILE`). Empty by default; a bad file stops the coder at startup (exit 78). [The chart](../../deploy/coder/README.md#model-profiles) |
| pictures for the model | no chart key: `config.extraEnv` carries `MODEL_INPUT_MODALITIES=text,image` and the `MODEL_IMAGE_*` limits ([environment](../reference/environment.md)); off by default, and the chart renders what it always did. The coder keeps the pictures on its volume per conversation ([the coder](../reference/coder-agent.md#images)) |
| `runPods.enabled` | [below](#run-pods). Production runs the commands of a run in run pods; [an example of the values](../../deploy/coder/examples/production.values.yaml) |
| `runPods.allowCommandsNextToSecrets` | `false` (default). `true` accepts that, with an App and no run pods, a command can read the App's key file; only for an upgrade you cannot finish yet: [upgrading](#upgrading-an-app-deployment) |

Render-time guards stop a bad combination at `helm template`, not at rollout (`templates/_validate.tpl`).

::: in-depth Why the contract release is a one-time window

## The contract release: a one-time window

ADR 0055 shipped in two releases. The **expand** release (S0–S2b) writes the whole v4 `state`
beside the head, so a v4 process and a v5 one can both step a run; the **contract** release (S3)
writes `state = null` (the tombstone), the head and the body, and every read serves the head. A v4
process that touches a converted run **fails it**: its `Envelope::decode` refuses a head, so it
commits the run `Failed` with `state = null`, no head and the version bumped. The run is **terminal**
and stays so (`sched_at` is `None` for a failed run, so no worker claims it): the contract's reads
serve its head, and `start_continuing` (or `start_with_id_continuing`) can carry it into a new run;
nothing resumes it in place. There is no fence inside a v4 binary that would make it refuse first.

So the rollout from an expand-release image to a contract-release image is a **deploy boundary**, not
an ordinary `helm upgrade`:

1. Confirm no process still runs an expand-release image against this database — the front and every
   worker. With the default chart (one StatefulSet, one replica) `RollingUpdate` deletes the pod
   before it creates the next, so the old image is gone before the new one starts. With
   `topology: split` the front is a **Deployment** whose default `maxSurge: 1` starts the v5 pod and
   waits for it to be Ready **before** it terminates the v4 one: scale it out of the way first,
   `kubectl -n another-agentic-system scale deploy <fullname>-front --replicas=0` (the chart names it
   `<fullname>-front`: `coder-front` for a release named `coder`). The `helm upgrade` in step 2
   re-applies `front.replicas` and brings the front back on the new image, so there is nothing to
   scale up by hand; the manual scale is only the "before" half. A deployment that ran
   more than one worker, or a pinned old image somewhere else, must be scaled to zero the same way.
2. Roll the new image (`helm upgrade --install …`). The store's `migrate()` runs first and is
   idempotent; PostgreSQL's schema is version 6.
3. Do not roll back to the expand release after a run has committed under the contract: its `state`
   is `null` and only a v5 binary reads it. A rollback is safe only for runs the contract has not
   converted.

Do **not** merge the contract and roll it in one step when your deployment renders this chart from
`main`: the merge bumps `image.tag`, so the roll would follow the merge by itself. Pin `image.tag`
(and digest) in your own values **before** merging, merge, then move the pin by hand after the old
processes are gone — the chart's bump is a convenience for a deployment that does not need the
boundary.

:::

## Verify

* The pods are `Ready` (probes hit `/healthz`; the GitHub MCP sidecar starts first).
* `GET <PUBLIC_URL>.well-known/agent-card.json` is public and returns the card; so are Swagger UI and the OpenAPI
  document; any other route without a bearer token is `401`.
* Swagger UI has no Ingress either: `kubectl -n another-agentic-system port-forward svc/coder 8080:8080`, then open
  `http://127.0.0.1:8080/docs`, **Authorize** with a token and **Try it out**
  ([the chart](../../deploy/coder/README.md#swagger-ui)).
* Send a task as in [Run it locally](run-locally.md#send-the-coder-a-task) with a real repository.
* A pod that exits **78** has a configuration problem (the log names the variable; an App with a key file and no run pods says
  `refusing to start with the GitHub App's private key in a file`: [upgrading](#upgrading-an-app-deployment)); **69** means
  Postgres is not reachable; a missing `ExternalSecret` property leaves pods in `CreateContainerConfigError`.

Graceful shutdown gets 120 seconds: on SIGTERM the workers finish the steps they are in; a step cut short by
SIGKILL is taken over once its lease expires.

## Run pods

`runPods.enabled: true` ([ADR 0019](../decisions/0019-a-runs-processes-in-a-pod-of-their-own.md)) runs each active
run's checks and commands in **a pod of its own**, so a build is bounded by a pod's memory (2Gi) and
not the coder's (about 1Gi). Off by default, and then invisible. It renders the pod template (a ConfigMap), a
`PriorityClass` and a `ResourceQuota` scoped to it, a ServiceAccount with a Role for `pods` and `pods/exec`, a
`ValidatingAdmissionPolicy` that refuses any pod of that ServiceAccount but the intended one, and a
`NetworkPolicy` that is **deny by default** ([ADR 0042](../decisions/0042-run-pods-reach-the-internet-through-an-egress-proxy.md)):
a run pod reaches DNS, the **egress proxy** the chart runs beside it (`adam-egress`, from the coder's own image) and
`runPods.networkPolicy.extraEgress`, and nothing else. The proxy lets a run pod reach the package registries and git hosts
of its allow-list over HTTPS (`CONNECT` to port 443, public IPv4 addresses only) and refuses every other host; the pod's
`HTTPS_PROXY` names it. Add a host with `runPods.egressProxy.extraAllow`; there is no switch for open egress. Details:
[chart README, "The egress proxy"](../../deploy/coder/README.md#the-egress-proxy). Needs the chart's floor, Kubernetes 1.37+. A run pod mounts only its own run's directory, each slot's directory on its own (so the pod cannot rename it), the mirror of each of its
repositories **read-only** and each worktree's own directory in it, and no Secret a container can read (the pod gets no model key, and its pull secrets are the chart's own: ADR 0038;
[ADR 0041](../decisions/0041-a-run-pod-sees-only-its-own-run.md), proposed): `git commit` and anything else that writes the
mirror fails in the pod, the coder commits. The template must mount the work claim once, whole, at `WORKSPACE_ROOT` (the
chart's does; a hand-written one that does not stops the coder, exit 78).

**A repository's own image, if you allow its registry.** `runPods.repositoryImages.allowedPrefixes` is empty by default: the
run pods use the template's image, and the coder never offers `use_repository_environment`
([ADR 0052](../decisions/0052-no-podman-a-repositorys-own-environment-is-a-run-pod-of-its-image.md)). Name one or more
prefixes (each ends with `/` for a registry path, `ghcr.io/acme/`, or `:` for a tag, `ghcr.io/acme/tool:`) and a repository
whose `devcontainer.json` names such an image may move its run pod to it. The chart renders `RUN_POD_REPOSITORY_IMAGES` and
admits a prefixed image **for the `run` container alone**; the init containers keep the exact list. The pod's user, mounts,
resources, network and runtime class do not move, and a configuration that only builds (a Dockerfile, features) or uses Compose
is refused in words. A bare host is refused, because `ghcr.io` would admit `ghcr.io.evil.com`; so are `*` and `${...}`.

**The coder's own git runs in a sandbox container.** `git` runs the programs a repository's configuration names (a clean
filter, an fsmonitor), and some repositories are a run's own, so with run pods the chart adds a second container to the coder
pod, `git-sandbox`, in which the `git status`, `git add`, `git diff` and `git commit` of the coder run
([ADR 0041](../decisions/0041-a-run-pod-sees-only-its-own-run.md), open decision 6; issue
[#118](https://github.com/vymalo/another-adam-rs/issues/118)). It has the work volume and the coder's user and nothing secret: no
Secret volume or variable, no service-account token (the token is a projected volume of the coder container alone), a process
namespace of its own and a read-only root file system. `fetch`, `push` and `ls-remote`, which need the token, stay in the coder.
There is nothing to configure but its memory (`runPods.gitSandbox.resources`, 1Gi) and no switch to turn it off; the coder rolls once on the upgrade.
**Without run pods git runs in the coder's container, next to the secrets, and is not protected.** What it does not cover (a program in the
sandbox still reads and writes the whole work volume) and how it is checked: [chart README, "The git sandbox"](../../deploy/coder/README.md#the-git-sandbox).

Details and what is unverified on the owner's cluster:
[chart README, "Run pods"](../../deploy/coder/README.md#run-pods). A repository's own environment is an image its run
pod runs, never a build ([ADR 0052](../decisions/0052-no-podman-a-repositorys-own-environment-is-a-run-pod-of-its-image.md)).

## Upgrading an App deployment

**A breaking change** ([vymalo/another-adam-rs#120](https://github.com/vymalo/another-adam-rs/issues/120), ADR 0036 decision 6).
With `github.auth: app` and `runPods.enabled: false`, the default, the commands of a run (`run_command`, `run`, `run_checks`)
run in the coder's own container as the user that can read the App's private key, a mounted file. A command can read it, mint
installation tokens and act as the App at **any access tier, a `plan` included**. The coder used to log a warning and go on. It
now **refuses to start** (exit 78), and the chart **refuses to render**.

| Your release | After the upgrade |
|---|---|
| `github.auth: token` | nothing changes |
| `github.auth: app` and `runPods.enabled: true` | nothing changes: the commands run in pods that have no key |
| `config.role: control-plane`, or the front of `topology: split` | nothing changes: it runs no command and holds no key |
| `github.auth: app` and `runPods.enabled: false` | **`helm template` fails** and names the values below |

Choose one, in the same change that moves `image.tag` (the chart's bump):

1. **Turn on run pods** (the safe one): `runPods.enabled: true`. It needs the chart's floor, Kubernetes 1.37 or later,
   the right to make the cluster-scoped objects the chart renders (a `PriorityClass`, a
   `ValidatingAdmissionPolicy` and its binding); [Run pods](#run-pods) and the
   [chart README](../../deploy/coder/README.md#run-pods) have the rest, and
   [`deploy/coder/examples/production.values.yaml`](../../deploy/coder/examples/production.values.yaml) is a complete set of values
   (an App by owners, the key a file, run pods on) that the render checks assert.
2. **Accept the risk for now**: `runPods.allowCommandsNextToSecrets: true`. The coder starts as before and still logs the warning
   at every start. Remove the line when run pods are on. Grant the App only the permissions you can live with meanwhile
   ([the tiers](../../deploy/coder/README.md#what-the-github-app-must-be-granted)).

The render failing is the point: with Argo CD or another GitOps tool the sync fails and **the running pods stay**, where a rollout
would replace the coder with one that exits 78 in a loop. Run `helm template` on your values before you merge.

Things to know:

* The chart sets the variable itself, and only when the value is `true`: `config.extraEnv.ALLOW_COMMANDS_NEXT_TO_SECRETS` is
  refused. If you set it there to get ahead of the upgrade (an older chart accepts it, an older image ignores it), move it to the
  value in the upgrade.
* A deployment that pins `image.tag` to a new image under an **older chart** has no value to set and crash-loops: set
  `ALLOW_COMMANDS_NEXT_TO_SECRETS=true` in `config.extraEnv` first, or move the chart first.
* Under an operator (see the platform repository, https://github.com/vymalo/another-agentic-platform/blob/main/deploy/operator/README.md), an `AgentConfig` with
  `github.app` and no run pods needs `spec.extraEnv.ALLOW_COMMANDS_NEXT_TO_SECRETS: "true"` before its image moves to this
  revision, or the pod exits 78.
  Compose and any other deployment set the variable themselves.
* A key given **inline** (`GITHUB_APP_PRIVATE_KEY`) is not a file and is not refused. The chart never uses it.
* To see whether a release is affected, run `helm template` with its values: it fails if it is. A pod that is already failing
  says `refusing to start with the GitHub App's private key in a file` in its log.

::: in-depth Upgrading from the devcontainer variables

## Upgrading from the devcontainer variables

[ADR 0052](../decisions/0052-no-podman-a-repositorys-own-environment-is-a-run-pod-of-its-image.md) removed the
repository's devcontainer on a container service, with no switch. A deployment that still sets
`DEVCONTAINER_RUNTIME=podman` in `config.extraEnv` **stops the coder** (exit 78, naming the variable and the decision):
remove it and `CONTAINER_HOST` before moving `image.tag`. Every other removed variable (`DEVCONTAINER_*`) is ignored,
with one warning at startup that names it. A repository that needs its own tools names an image its run pod runs
(`runPods.repositoryImages.allowedPrefixes` and `use_repository_environment`, [Run pods](#run-pods)).

:::

## A package cache shared by pods

By default every pod starts with an empty home and downloads its packages again. `cache.existingClaim` (a
ReadWriteMany claim you manage, shared with other releases) or `cache.create` mounts one volume at `/cache` in the
coder pods and the run pods, where npm, pnpm and cargo keep their caches
([ADR 0040](../decisions/0040-a-shared-package-cache-at-cache.md)). Off by default, and then invisible. It is one
trust domain, cargo takes no lock on NFS, and the volume's root must be writable by uid 10001:
[chart README, "The package cache"](../../deploy/coder/README.md#the-package-cache).

## A folder agent instead of the coder

The same image carries `adam-agent`. The chart deploys the coder only. For a folder agent, run the image with
the entrypoint `tini -- adam-agent`, the folder mounted read-only at `ADAM_AGENT_DIR` and readable by uid 10001,
and write your own manifest using the chart's StatefulSet as a model for the variables
([Write an agent](write-an-agent.md), [`bin/adam-agent`](../../bin/adam-agent/README.md#image-and-compose)).
Or let an operator make it from an `AgentService`: the operator lives in
[vymalo/another-agentic-platform](https://github.com/vymalo/another-agentic-platform/blob/main/deploy/operator/README.md).

## How a change reaches the chart

```mermaid
sequenceDiagram
    autonumber
    participant Dev as Pull request
    participant CI as coder.yml
    participant GH as ghcr.io
    participant Chart as deploy/coder/values.yaml

    Dev->>CI: helm lint and template, kubeconform, hadolint, shellcheck
    Dev->>CI: build the image and run the container smoke test
    CI-->>Dev: green
    Dev->>CI: merge to main
    CI->>CI: build the image and smoke-test it again
    CI->>GH: push ghcr.io/vymalo/another-adam-rs/coder, tag sha-XXXXXXX
    CI->>Chart: set image.tag, commit "chore(deploy): bump coder to sha-XXXXXXX"
```

Pin a consumer to an image by tag **and** digest (`adam-upgrade`). The build context is the repository root.
A new GHCR package is private until its owner makes it public: check an anonymous pull before pointing a
deployment at it.

`sha-<7>` is for development. Production deploys a **release**: the image `X.Y.Z`, built, smoke-tested and signed on
the tag `vX.Y.Z`, and the chart `oci://ghcr.io/vymalo/another-adam-rs/charts/coder` at the same version, whose
`image.tag` is already `X.Y.Z`. `values.yaml` on `main` never holds a release. How one is cut, which tag to pin and how
to check it: [Releases](releases.md).

## Known risks

* **No database backups.** Losing the CNPG volume loses the run ledger, not the pushed branches or pull requests.
* **A pinned run whose worker never returns is stranded** (`affinity`, `isolated`): keep worker ids stable, do not
  scale in, keep the volumes.
* **`flock` on network volumes is unverified** for `shared` on NFS and Longhorn RWX.
* **The chart has not been applied with more than one worker**; the renders are checked, a live rollout is not.
* The `NetworkPolicy` is inert unless the cluster's CNI enforces policies. With run pods that means the egress proxy is there and nothing forces a run pod through it (kind's kindnet enforces them: the `run-pods` CI job fails where they are not).
* **Run pods reach only the hosts of `runPods.egressProxy.allow`** (HTTPS). A tool that ignores `HTTPS_PROXY`, or fetches over plain HTTP or from a host that is not listed, fails; an allowed host still accepts what a run sends it with its own credential, and domain fronting is not seen ([ADR 0042](../decisions/0042-run-pods-reach-the-internet-through-an-egress-proxy.md)).
* The model gateway's URL is not a credential: kept out of git, it is still in the pod's environment and the
  startup log.

Full list and the verified/unverified facts: [chart README](../../deploy/coder/README.md#known-risks).
