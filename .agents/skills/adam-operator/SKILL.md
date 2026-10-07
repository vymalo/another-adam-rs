---
name: adam-operator
description: "Run adam-rs agents on Kubernetes as AgentService and AgentConfig custom resources (agents.vymalo.com/v1alpha1) with the adam-operator, instead of the coder Helm chart: install the CRDs and operator charts, write the two resources, reference secrets, read the status conditions, serve the agent registry, move from the coder chart. Use for 'run an agent with the operator', 'write an AgentService', 'AgentService is Blocked', 'NameConflict'."
---

# Run adam agents as custom resources

The operator turns two namespaced custom resources into a running agent. An `AgentConfig` says what
runs (the binary, the agent folder or the coder's settings, the model, the tools, the image). An
`AgentService` says how it runs (replicas, the Postgres store, who may reach it, the registry
entry) and names its config. For each service the operator makes the workload, a Service, the files
and the policy, and lists the agent in the registry. One image, `ghcr.io/vymalo/another-adam-rs/operator`
(binary `adam-operator`), and two Helm charts. The API is alpha (`v1alpha1`); the group is
unchanged from the platform repository it came from (ADR 0029).

Every adam-rs path below is in `vymalo/another-adam-rs` at the revision you pin (replace `main` by
that revision): the CRD schema, the charts and the READMEs change between revisions. Entry point:
https://github.com/vymalo/another-adam-rs/blob/main/deploy/operator/README.md. The human version of
this page: https://github.com/vymalo/another-adam-rs/blob/main/docs/guides/run-agents-with-the-operator.md.

## When to use

* Running a folder agent (`adam-agent`) or the coder (`adam-coder`) on Kubernetes from custom resources:
  adding one, changing one, finding out why one is not `Ready`, or moving the coder off its Helm chart.
* Not for the chart itself (`adam-coder-deploy`), nor for the folder's content (`adam-agent-folder`).
* Not for run pods (ADR 0019): the operator makes none yet (ADR 0029, "What is missing"); the chart does.
* Proof so far: the kind jobs of `.github/workflows/operator.yml` (the coder under an `AgentService`, the binary, the CRDs, the
  provider, the CloudNativePG store) passed on `main` at `2644008` (*verified 2026-10-07*, the GitHub Actions API). The repository
  holds no production evidence; each README has a "What is not proven" section, and ADR 0029 an *Amended 2026-10-07* note.

## Procedure

1. **Install the CRDs as a release of their own** (`deploy/operator-crds`, from a checkout of adam-rs at your rev):

   ```sh
   helm template adam-operator-crds deploy/operator-crds | kubectl apply --server-side -f -
   ```

   Server-side, because the file is about 55 KB. Give them their own Argo CD Application, synced before the
   operator, and **never prune them**: deleting a CRD deletes every `AgentService` and `AgentConfig`, and
   the finalizers no longer run (`deploy/operator-crds/README.md`, "Removing it", which advises `Prune=false`;
   that sync option's behaviour is *unverified* here). Without the CRDs the operator starts and stays
   unready: `/readyz` is 503 until both caches have listed.
2. **Install the operator** (`deploy/operator`): one replica, no leader election, **namespaced: a `Role`,
   never a `ClusterRole`, and no right on Secrets**. The values that matter, in `deploy/operator/values.yaml`:
   `image.tag` (`sha-<7>`, written by CI in adam-rs: set your own, with `image.digest`), `watchNamespace` (one name, default the
   release's), `storeCnpg` (`false` without CloudNativePG), `registry.tokenSecret` or `externalSecrets` (the
   registry's token, step 8) with `networkPolicy.registry.allowFrom`, which is then required.
   `deploy/operator/examples/netcup.values.yaml` is one cluster's example.
3. **Write the `AgentConfig`.** Required: `harness` (`type: adam-rs`, `adam.binary` `adam-agent` or
   `adam-coder`, `adam.agent`), `model` (`model`, `baseUrl` as `value` or `secretRef`, `apiKeySecretRef`) and
   `environment.image.ref` (the coder image by tag and digest, `adam-upgrade`). `adam-agent` takes
   `agent.folder` (`files`, a map of path to content holding `instructions.md`, at most 1 MiB, or
   `configMapRef`) and no `coder` block. `adam-coder` takes `agent.embedded: {}` and the `coder` block
   (`github.app` or `github.token`, never both): `deploy/operator/examples/coder.yaml`. Optional: `tools`
   (`mcpServers`, `allowInsecureHttp`, `githubMcp`), `environment` (`resources`, `volumes`), `security`,
   `extraEnv`, `model.extraBody`.
4. **Write the `AgentService`.** Required: `configRef.name` (same namespace) and `store.postgres`;
   `interfaces.a2a` must be enabled with a token. A folder agent, trimmed from `deploy/operator/examples/chat.yaml`:

   ```yaml
   apiVersion: agents.vymalo.com/v1alpha1
   kind: AgentService
   metadata: { name: helper, namespace: agents }      # a DNS label of at most 52 characters
   spec:
     configRef: { name: helper }
     interfaces:
       a2a:
         enabled: true                                  # the default is false, and false is refused
         bearerTokensSecretRef: { name: helper-secrets, key: A2A_BEARER_TOKENS }
     store:
       postgres:
         secretRef: { name: helper-db, key: uri }       # or cnpg: { instances: 1, storage: { size: 5Gi } }
     registry: { title: Helper, tags: [chat] }
   ---
   apiVersion: agents.vymalo.com/v1alpha1
   kind: AgentConfig
   metadata: { name: helper, namespace: agents }
   spec:
     harness:
       type: adam-rs
       adam:
         binary: adam-agent
         agent:
           folder:
             files:
               instructions.md: |
                 ---
                 name: helper
                 description: A helper.
                 card: { name: Helper }
                 ---
                 Your name is Helper.
                 In one sentence: I help with small questions.
     model:
       model: my-model
       baseUrl: { value: https://gateway.example.invalid/v1 }
       apiKeySecretRef: { name: helper-secrets, key: MODEL_API_KEY }
     environment:
       image:
         ref: ghcr.io/vymalo/another-adam-rs/coder:sha-<7>@sha256:<digest>
   ```

   Also on the service: `scaling` (`topology` `combined` or `split`, `workers`, `front.replicas` only with
   `split`), `access.allowFrom` (NetworkPolicy peers: **an empty list makes no policy, so the pod is open to
   the cluster** and the bearer token is the gate), `deletionPolicy` (`Retain`, the default, or `Delete`),
   `suspend` (zero replicas, volumes kept), `interfaces.a2a.publicUrl` (default `http://<name>.<ns>.svc:8080/`).
   `interfaces.responses` and `interfaces.mcp` can only be false. The database role must be able to create
   tables: every role migrates at startup.
5. **Secrets are references, never values.** A field that needs one names a Secret and a key:
   `bearerTokensSecretRef` (comma-separated tokens; the orchestrator holds one), `store.postgres.secretRef`,
   `model.apiKeySecretRef`, `model.baseUrl.secretRef`, `github.token.secretRef`, `github.app.privateKeySecretRef`
   (mounted as a file) and an MCP header's `secretRef` (with an optional `prefix` such as `Bearer `). You create
   the Secrets. The operator has no right on them, so it never says whether one exists: a missing one is
   `MissingSecret`, from the pod. `extraEnv` and `model.extraBody` are plain values: nothing secret there.
6. **What it makes**, labelled `app.kubernetes.io/managed-by: adam-operator` and
   `app.kubernetes.io/instance: <name>`, annotated `agents.vymalo.com/config-digest`:

   | Object | Name | When |
   |---|---|---|
   | `StatefulSet` or `Deployment` | `<name>`, and `<name>-front` for the front of `split` | a StatefulSet with a per-replica volume or `affinity` or `isolated` placement |
   | `Service` | `<name>`, ClusterIP 8080 | always |
   | `ConfigMap` | `<name>-agent-<hash8>`, `<name>-mcp` | an inline folder; extra MCP servers |
   | `NetworkPolicy` | `<name>` | `access.allowFrom` is not empty (ingress only) |
   | claims | `<volume>-<name>-<n>` per replica, `<name>-<volume>` shared | a persistent volume |
   | `PodDisruptionBudget` | the front's name | `front.replicas` above 1 |
   | CloudNativePG `Cluster` | `<name>-db`, Secret `<name>-db-app` key `uri` | `store.postgres.cnpg` |

   A changed image, variable, file or volume is a rollout; `workers`, `suspend` and `allowFrom` are not. A
   `configMapRef` folder is tracked by name only: rename it to roll. Deleting a service runs the finalizer
   `agents.vymalo.com/runtime`: `Retain` keeps the claims and the CNPG cluster, `Delete` removes them, and no
   Secret is touched.
7. **Read the status.** `kubectl get agentservices` shows `State` (`Ready`, `Degraded`, `Suspended`, `Blocked`);
   `kubectl describe` shows the conditions `ConfigResolved`, `StoreReady`, `RuntimeReady`, `Listed`, `Ready` and
   the Events. `Blocked` means nothing was applied and what runs was left alone.

   | Reason | Meaning and fix |
   |---|---|
   | `ConfigNotFound`, `ConfigInvalid` | no such `configRef`, or a rule the schema cannot state (a2a off, a long name, a header, a path); the message lists every issue |
   | `NameConflict` | an object it needs (a Helm release's StatefulSet, say) is not its own. Nothing is written; it looks again every 15 s. Remove or rename that object |
   | `MissingSecret` | a Secret or key does not exist; the reason names the Secret. Create it: the pod recovers |
   | `ImagePull` | wrong tag, or a private package: the CRD has no `imagePullSecrets`, so make the package public |
   | `ConfigRejected`, `DependencyUnavailable`, `CrashLoop` | exit 78 (configuration: read the pod log), exit 69 (Postgres or a required MCP server is down), any other crash |
   | `CNPGNotInstalled`, `ClusterNotReady` | `store.postgres.cnpg` without CloudNativePG (or `storeCnpg: false`), or the cluster is starting |
   | `RegistryDisabled`, `RegistryFull` | `Listed` only: no registry token, or over 500 items or 1 MiB. It never gates `Ready` |

   A CEL refusal fails `kubectl apply` at once with the rule's message. Each rule has an invalid example in
   `deploy/operator/examples/invalid/`, whose `# expect:` line is that message.
8. **The registry.** With a token (`registry.tokenSecret` or `externalSecrets`: random, 32 bytes or more) the
   operator serves `GET /registry/v1/agents` on the Service `adam-operator-registry`, port 8080, with
   `Authorization: Bearer <token>`: an `agent-registry/v1` linkset of every service that has A2A on, a card URL and is
   not `Blocked` (a `Degraded` one is listed), each item with `href` (its card URL), `service` (its name), `title` and `tags`. No token, no
   registry. Past 500 items or 1 MiB it answers 503 rather than truncate. The orchestrator reads it with that
   token (its `AGENT_REGISTRY_URL` and `AGENT_REGISTRY_TOKEN`) and sends one agent token to every agent, which
   must be a member of each service's `A2A_BEARER_TOKENS`: the operator cannot check that. The contract is in the
   platform repository,
   https://github.com/vymalo/another-agentic-platform/blob/cfd03db836b39fd4ff81ac0275f2602781ab19c4/docs/extensions/agent-registry-v1.md
   (*verified to exist 2026-10-07*, GitHub contents API; its content is not restated here).
9. **Move from the coder chart.** A hard cutover. Write the pair from the release's values
   (`deploy/operator/examples/coder.yaml` maps them); name the service `coder` and point `store.postgres.secretRef`
   at the chart's database Secret (CloudNativePG's `<cluster>-app`, key `uri`) so the runs survive. Prune the
   Helm release: its StatefulSet, Service and PVC claim are `coder` and `work-coder-0` for a release named
   `coder`, and while they stand the service is `NameConflict`. Apply the pair. The operator's StatefulSet mounts
   `work-coder-0` by name, so the workspace is adopted; delete the claim first to start clean. That a
   StatefulSet reuses a claim another one made is *unverified* here.

## Verify

* The operator pod is `Ready`, `kubectl get agentservices -n <ns>` says `Ready`, the Events hold `Reconciled`.
* The card is public at `http://<name>.<ns>.svc:8080/.well-known/agent-card.json`; any other call without a
  bearer token is 401. The registry answers 200 with its token, 401 without.

## Pitfalls

* Installing the operator before the CRDs, or pruning the CRDs release.
* Leaving `interfaces.a2a.enabled` at its default (`ConfigInvalid`), or expecting an empty `access.allowFrom` to deny.
* Editing `deploy/crds/agents.vymalo.com.yaml` by hand: `adam-operator crdgen` writes it, and
  `deploy/operator-crds/files/` holds a checked copy. Setting the operator chart's `image.tag` by hand: CI writes it.
* Rotating the registry token's Secret without restarting the operator: it reads the file once.
* Expecting run pods, a `ClusterRole`, several operator replicas or several watched namespaces: none exists.

## See also

* `deploy/operator/README.md`, `deploy/operator-crds/README.md`, `bin/adam-operator/README.md`,
  `crates/adam-operator-controller/README.md` (the status), `crates/adam-operator-runtime-kubernetes/README.md`
  (the objects), `crates/adam-operator-registry/README.md`, `docs/decisions/0029-adam-rs-has-an-operator.md`.
* `adam-agent-folder`, `adam-coder-deploy`, `adam-a2a-extensions`, `adam-upgrade`.
* https://github.com/vymalo/another-adam-rs/blob/main/deploy/operator/examples/coder.yaml
