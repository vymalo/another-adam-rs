# adam-operator

> Moved into adam-rs from `vymalo/another-agentic-platform` at commit [`cfd03db`](https://github.com/vymalo/another-agentic-platform/tree/cfd03db836b39fd4ff81ac0275f2602781ab19c4) ([ADR 0029](../../docs/decisions/0029-adam-rs-has-an-operator.md)). `§N`, `AD-NNN`, `Sn` and `M0`..`M3` in this file cite that repository's design documents (`docs/architecture`, `docs/mvp.md`) as of that commit, which is archived; they are not links into this one.

The composition root of the operator (AD-020), binary `adam-operator`. It names the types the libraries leave generic: the
Kubernetes runtime provider, the store provisioner and the controllers. Nothing else is in it.

```sh
cargo run -q -p adam-operator -- crdgen > deploy/crds/agents.vymalo.com.yaml   # regenerate the CRDs
cargo run -q -p adam-operator -- run                                            # the controllers, against KUBECONFIG or the cluster
```

| Subcommand | What |
|---|---|
| `crdgen` | Prints the CRDs of [`adam-operator-api`](../../crates/adam-operator-api/README.md) as multi-document YAML on standard output, behind a two-line header that says it is generated |
| `run` | Composes `KubernetesRuntime` ([`adam-operator-runtime-kubernetes`](../../crates/adam-operator-runtime-kubernetes/README.md)), `CnpgStore` ([`adam-operator-store-cnpg`](../../crates/adam-operator-store-cnpg/README.md), which also serves a referenced Secret; `SecretStore` ([`adam-operator-store-secret`](../../crates/adam-operator-store-secret/README.md)) without the `store-cnpg` feature) and the [`Operator`](../../crates/adam-operator-controller/README.md), serves health and metrics, and runs until SIGTERM or SIGINT, then lets the passes in flight finish |

Libraries use `thiserror`; this binary uses `anyhow` (and has no error type of its own). **No leader election, one
replica** (§59a: "One replica, `Recreate`, no leader election"): a second reconciler would only race the first.

## `run`: settings

Each is a flag and an environment variable, which is how the chart sets them (S8).

| Variable | Flag | Default | Meaning |
|---|---|---|---|
| `WATCH_NAMESPACE` | `--watch-namespace` | none: every namespace | Watch one namespace (the namespaced operator of §93). Empty is the same as unset |
| `HEALTH_ADDR` | `--health-addr` | `0.0.0.0:8081` | `/healthz`, `/readyz` |
| `METRICS_ADDR` | `--metrics-addr` | `0.0.0.0:9090` | `/metrics` |
| `POD_NAME` | `--instance` | none | The `reportingInstance` of Events |
| `ADAM_OPERATOR_CONCURRENCY` | `--concurrency` | 4 | Services reconciled at once (never the same one twice) |
| `ADAM_OPERATOR_RESYNC_SECS` | `--resync-secs` | 300 | Timer for a service that is Ready or Suspended |
| `ADAM_OPERATOR_RESYNC_PENDING_SECS` | `--resync-pending-secs` | 15 | Timer for one that is rolling out, unwell or held by a foreign object |
| `REGISTRY_ADDR` | `--registry-addr` | `0.0.0.0:8080` | Where the agent registry is served (feature `registry`, and only with a token) |
| `REGISTRY_TOKEN_FILE` | `--registry-token-file` | none | The file that holds the registry's bearer token (a mounted Secret), read once at start. **No token, no registry**: unset, missing, unreadable or empty, nothing is served on the port, the operator logs why and keeps reconciling, and every service is `Listed: False`, reason `RegistryDisabled` |
| `REGISTRY_PUBLIC_URL` | `--registry-public-url` | none | The URL of the registry document itself, sent as its `anchor` (optional in the contract) |
| `RUST_LOG` | | `info` | `tracing` filter; logs go to standard error |
| `KUBECONFIG` | | | Used when there is no in-cluster environment (`kube::Client::try_default`) |


* `GET /healthz` on 8081: 200 while the process is up (liveness).
* `GET /readyz` on 8081: 200 once both controllers' caches have listed the cluster, 503 before.
* `GET /registry/v1/agents` (and `HEAD`) on 8080, with `Authorization: Bearer <token>`: the [agent registry](../../crates/adam-operator-registry/README.md), `401` with no body without the token, `503` before the caches have synced or past its limits. Nothing else is served there.
* `GET /metrics` on 9090: Prometheus text, the counters listed in the [controller's README](../../crates/adam-operator-controller/README.md#metrics).

### Features

| Feature | Default | What |
|---|---|---|
| `runtime-kubernetes` | yes | Builds `run` with the Kubernetes provider. Without it `run` exits 1 and says to rebuild with it; `crdgen` still works |
| `registry` | yes | Serves the agent registry on 8080 ([`adam-operator-registry`](../../crates/adam-operator-registry/README.md)), sets `Listed` to `True` / `Listed` for what it lists, and builds its document every 15 s so `RegistryFull` follows the fleet. Without it nothing is served on 8080 and `Listed` says `RegistryDisabled` |
| `store-cnpg` | yes | The store is `CnpgStore`: a service with `store.postgres.cnpg` gets a CloudNativePG `Cluster`. Without it the store is `SecretStore` and such a service is `StoreReady: False`, reason `CNPGNotInstalled` (the same words as a cluster without CloudNativePG) |

§59a lists `runtime-kubernetes`, `store-cnpg` and `registry`, all three declared. The controller never names the store type: `run` picks `CnpgStore` or `SecretStore` by
the feature, and the controller is generic over `StoreProvisioner` (AD-020).

## Cluster rights

Everything `run` does to the API, for the chart of S8 (a namespaced `Role`, §59a): the
[controller's](../../crates/adam-operator-controller/README.md#cluster-rights) plus the
[provider's](../../crates/adam-operator-runtime-kubernetes/README.md#cluster-rights), and with `store-cnpg` the
[store's](../../crates/adam-operator-store-cnpg/README.md#cluster-rights). **No right on Secrets.**

## The CRDs are a checked-in file

[`deploy/crds/agents.vymalo.com.yaml`](../../deploy/crds/agents.vymalo.com.yaml) is the output of
`crdgen`, byte for byte. Change a type in `adam-operator-api`, regenerate, commit both. Two checks hold it:
`cargo test -p adam-operator` (`tests/cli.rs`) and the `crds-drift` job of
[`.github/workflows/operator.yml`](../../.github/workflows/operator.yml).

## Tests

```sh
cargo test -p adam-operator                                  # the cluster cases skip
ADAM_OPERATOR_TEST_KUBECONFIG=$HOME/.kube/config ADAM_OPERATOR_TEST_STUB_IMAGE=adam-operator-stub:ci cargo test -p adam-operator --test cluster
```

| File | What |
|---|---|
| `src/serve.rs` unit tests | `/healthz` is up at once, `/readyz` answers 503 until the readiness handle is marked, `/metrics` is Prometheus text |
| `tests/cli.rs` | runs the built binary: `crdgen` equals the checked-in file and prints one document per kind; `run` without a cluster fails and says it was connecting; `run --help` lists every variable above |
| `tests/cluster.rs` | **the end-to-end**: the binary against a real cluster (below) |

### The end-to-end, `tests/cluster.rs`

Every case starts `adam-operator run` (the binary Cargo built, `CARGO_BIN_EXE_adam-operator`) against the cluster of
`ADAM_OPERATOR_TEST_KUBECONFIG`, with `WATCH_NAMESPACE` set to a namespace of its own, free ports, and its log in
`$TMPDIR/adam-operator-e2e-*.log` (printed when a case fails). The CRDs are installed first by server-side apply (what `crdgen`
prints, and the `operator-e2e` job also applies the checked-in file with `kubectl`). **Everything a case creates has a name
of its own** (the namespace, the Secrets, the objects): the cases run in parallel in one cluster, the lesson of S4's
first CI run.

The pods run [`tests/stub/Dockerfile`](tests/stub/Dockerfile): busybox pinned by digest that answers `/healthz` on 8080, with
`adam-coder` as its entrypoint and `tini` and `adam-agent` where the operator puts them (`tini -- adam-agent`). **No adam
image is needed**; CI builds it and loads it into kind. The CRD has no command override, so the stub is the way a test
image runs as the real binaries would.

| Case | What it shows |
|---|---|
| `a_folder_agent_is_made_becomes_ready_and_its_deletion_completes` | an `adam-agent` service becomes `Ready` with every condition, `status.runtime`, endpoints and the digest; the Deployment carries our labels, an owner reference to the service and the digest on its pod template; the Service and the folder's ConfigMap exist; the config is `Valid`; an Event `Reconciled`; deleting completes the finalizer and the Deployment, Service and ConfigMap are gone, the config stays |
| `a_coder_keeps_its_volume_under_retain_and_loses_it_under_delete` | an `adam-coder` with a per-replica volume is a StatefulSet with its claim template; deleted, `Retain` leaves the claim (without an owner) and `Delete` removes it |
| `a_missing_secret_is_a_condition_until_the_secret_exists` | the Secrets do not exist: `RuntimeReady: False`, reason `MissingSecret`, `Degraded`, no value in the status; the Secrets appear and the service becomes `Ready` with no object of ours changed (the runtime's watch) |
| `a_missing_or_invalid_config_blocks_and_a_fixed_one_unblocks` | `ConfigNotFound` and nothing made; an invalid config is `ConfigInvalid` and the config is not `Valid`; fixing it makes the service Ready; breaking it again leaves the running Deployment untouched |
| `suspend_scales_to_zero_and_resume_wakes` | `Suspended` with zero replicas, then `Ready` again |
| `an_object_that_is_not_ours_with_the_name_is_a_conflict_that_changes_nothing` | a Deployment named like the service, labelled `Helm`: `NameConflict`, `Blocked`, the foreign object not written; its removal lets the operator make its own |
| `a_deletion_that_happens_while_the_operator_is_down_completes_when_it_returns` | the operator is killed (SIGKILL), the service deleted: the finalizer holds the object and the Deployment; a new operator completes it |
| `the_registry_lists_a_ready_agent_to_whoever_holds_the_token_and_nobody_else` | S7: with a token file the operator serves the registry: `401` with no body for no token, a wrong one and the wrong scheme; `200` with the contract's headers, the `Ready` agent as an item with its card URL, title and tags, and the `Blocked` one not listed (`Listed: False` / `ServiceBlocked`); `304` on a match; `HEAD`; and, when `ADAM_OPERATOR_TEST_HOST_ADDR` is set, the registry and the card it lists read **from a pod** (and refused there without the token); a deleted service leaves the list |
| `without_a_token_no_registry_is_served_and_nothing_is_listed` | S7: no `REGISTRY_TOKEN_FILE`: nothing listens on the registry's port and the service is `Listed: False` / `RegistryDisabled` |
| `a_service_that_asks_for_a_cluster_is_cnpg_not_installed_without_cloudnativepg` | S6: `store.postgres.cnpg` on a cluster without CloudNativePG is `StoreReady: False`, `CNPGNotInstalled`, `Blocked`, no workload, and the deletion of that service completes (a skip when CloudNativePG is installed; a failure under `ADAM_OPERATOR_TEST_REQUIRE_CLUSTER=1`) |
| `health_readiness_and_metrics_are_served` | `/healthz`, `/readyz` 200 and the counters on `/metrics` |
| `the_manifests_are_valid_and_resolve` | the cases' own objects pass `adam_operator_domain::resolve`, with no cluster, so a broken fixture is not found minutes into CI |

| Variable | Meaning |
|---|---|
| `ADAM_OPERATOR_TEST_KUBECONFIG` | the kubeconfig file of the cluster to test. **Unset: the cluster cases skip.** Never the default context |
| `ADAM_OPERATOR_TEST_REQUIRE_CLUSTER` | `1` or `true`: unset `ADAM_OPERATOR_TEST_KUBECONFIG` is a failure (CI) |
| `ADAM_OPERATOR_TEST_STUB_IMAGE` | the stub image, already on the cluster's nodes. Default `adam-operator-stub:ci` |
| `ADAM_OPERATOR_TEST_HOST_ADDR` | the address of this machine as the cluster's pods reach it (kind: the gateway of the docker network `kind`; the workflow computes it). Set, the registry case also reads the registry and the card from a pod. Unset, that part is skipped |
| `ADAM_OPERATOR_TEST_NO_WORKLOADS` | `1` or `true`: the cluster is only an API server and etcd (no controller manager, no kubelet). The test then patches the status of every Deployment and StatefulSet to "rolled out" itself and skips the cases that need a kubelet. See below |

### What has and has not been run

* **The `operator-e2e` job of `operator.yml` has not run.** No kind, docker daemon or kubelet existed where this slice was
  written. Everything below that needs pods (the claims of a StatefulSet, the kubelet's `CreateContainerConfigError`, a real
  rollout, the stub image) is *unverified* until CI has run it.
* **What was run**: on 2026-10-05, the nine cases (the missing-Secret case returns early) against a bare **kube-apiserver
  v1.35.8 and etcd v3.5.21** started by hand (the kind job's Kubernetes version; no controller manager, no kubelet), with
  `ADAM_OPERATOR_TEST_NO_WORKLOADS=1`: all passed. That proves the operator binary, the controllers, the provider's server-side
  apply, the finalizer, the status subresource, Events, list and watch, and the CRDs and their CEL rules against a real API
  server. It does **not** prove that a pod starts, that the kubelet words a missing Secret as the provider expects, that
  the StatefulSet controller makes the claims, or the stub image.
  The run found one thing: a pass on a stale cache fails the finalizer's `test` with a 422, which the first version of the
  controller classed as bad input (10 minutes of waiting); it is a lost race and is now a `Conflict`.
* **S6, 2026-10-05**, the same bare kube-apiserver v1.35.8 (etcd v3.6.4), all ten cases with `ADAM_OPERATOR_TEST_NO_WORKLOADS=1` and
  `ADAM_OPERATOR_TEST_REQUIRE_CLUSTER=1`: they passed. The first run of the new case failed **all five cases that delete a service**:
  with `store-cnpg` the finalizer's release asked for a Cluster, and an API server without CloudNativePG answers a plain-text
  `404 page not found` that `Api::get_opt` does not take for "not found". That is fixed in `adam-operator-store-cnpg` and held by
  its fake API server and by this case.
* **S7, 2026-10-05**, the same bare kube-apiserver, all twelve cases: they passed, the registry case's host-side part
  included (the token, the headers, the item, `304`, `HEAD`, a Blocked service not listed, a deleted one gone). **Its pod
  part has not run** (`ADAM_OPERATOR_TEST_HOST_ADDR` is unset there: no pod ever exists on a bare API server), and neither has
  the kind job: *unverified* until `operator-e2e` has run.
