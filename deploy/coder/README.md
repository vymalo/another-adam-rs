# coder chart

The coder agent (`crates/adam-coder`) as one StatefulSet, with its own
CloudNativePG database, for the `netcup-k8s` cluster.

| Object | What |
|---|---|
| `StatefulSet` | 1 replica (MVP), PVC `work` at `/work` on `longhorn` (mirrors and worktrees persist), `fsGroupChangePolicy: OnRootMismatch`, uid/gid 10001, probes on `/healthz` |
| `Service` | ClusterIP only. **No Ingress**: the orchestrator reaches it in-cluster over A2A with a bearer token |
| `NetworkPolicy` | ingress only from the namespace `another-agentic-system`; egress open (git, the gateway, registries) |
| `Cluster` (CNPG) | the coder's database; `DATABASE_URL` is the `uri` key of the `<release>-db-app` Secret CNPG creates |
| `ExternalSecret` | `ssegning-aws` / `prod/meta/test-app`; the property of each value is in `externalSecrets.properties` |

```sh
helm lint deploy/coder
helm template coder deploy/coder --namespace another-agentic-system
sh deploy/coder/tests/render-check.sh   # the guarantees above, asserted on the render
```

`image.tag` is bumped by `.github/workflows/coder.yml` on every green build of
main (`deploy/coder/bump-tag.sh`, also run as a dry run on pull requests).

## Secrets

| Env | AWS property (default) |
|---|---|
| `MODEL_API_KEY` | `adam_coder_model_api_key` |
| `GITHUB_TOKEN` | `adam_coder_github_token` |
| `A2A_BEARER_TOKENS` | `adam_coder_a2a_bearer_tokens` (comma-separated) |

The orchestrator holds one of the bearer tokens (its agent list names the
environment variable it reads it from).

## Repositories

`config.allowedRepoHosts` (default `github.com`; env `ALLOWED_REPO_HOSTS`) lists
the hosts a task may name a repository on. `GITHUB_TOKEN` is only ever sent to
those hosts: any other host, a local path or a URL with embedded credentials is
refused before git runs. For GitHub Enterprise add its host and set
`config.githubApiUrl` (env `GITHUB_API_URL`, default `https://api.github.com`) to
`https://<host>/api/v3`. `ALLOW_LOCAL_REPOS` is for development and tests and is not
exposed by the chart.

## Known risks

* **No database backups.** Losing the CNPG volume loses the run ledger (what
  is running, what finished), not the pushed branches or pull requests.
* **One replica.** Worktrees live on a ReadWriteOnce volume, and the workspace
  layer assumes one process per workspace root.
* The `NetworkPolicy` depends on the cluster's CNI enforcing policies (it is
  otherwise inert) and on the namespace label `kubernetes.io/metadata.name`
  (set automatically by Kubernetes 1.21+).

## Verified and unverified

* `external-secrets.io/v1` is what the cluster's other charts use (checked in
  `WhyThatFunction/home-os`, 2026-09-29).
* The CNPG `Cluster` fields and the `<cluster>-app` Secret with a `uri` key are
  from the CloudNativePG documentation; not yet checked against the installed
  operator version on the cluster.
* Rendered with Helm v3.19.0 (`helm lint`, `helm template`); not applied to a
  cluster and not validated against the CRD schemas.
