# 0029. adam-rs has an operator

Status: **Accepted** (2026-10-07), on the owner's decisions of that day (below). **Built:** the crates, the binary, the
image, the two charts and the workflow named here. **Not built:** run pods (ADR 0019) in the operator, see *What is
missing*. Builds on [ADR 0019](0019-a-runs-processes-in-a-pod-of-their-own.md), whose chart `runPods` block the operator
does not yet reproduce.

## Context

The operator of `AgentService` and `AgentConfig` (`agents.vymalo.com/v1alpha1`) was written in
`vymalo/another-agentic-platform` as slice S1 to S9 of that repository's v0 (its design: §59a of
`docs/architecture/10-control-plane-and-crds.md`, AD-020 to AD-024, `docs/mvp.md`). It makes `adam-coder` and
`adam-agent` pods from two custom resources, so everything it knows about adam is a copy of this repository's
environment contract, held equal by *parity goldens* rendered from `deploy/coder`. The two repositories drifted: the
goldens pinned `0391809` while this repository went on (`MODEL_EXTRA_BODY`, run pods, Adam). No `AgentService` exists on
any cluster yet.

## Decision

The owner, 2026-10-07:

1. **adam-rs ships its own operator.** The platform repository is archived; adam-rs is where the operator lives, is
   tested against `deploy/coder` and is released with it.
2. **Copy with provenance, no history rewrite**, from platform commit
   [`cfd03db`](https://github.com/vymalo/another-agentic-platform/tree/cfd03db836b39fd4ff81ac0275f2602781ab19c4) (the
   platform repository was not modified).
3. **Rename `aap-*` to `adam-operator-*` everywhere**: crates, the binary and its subcommands, environment variables,
   metrics, labels, field managers, log targets (below). Nothing is running, so renaming what a cluster would remember
   is safe.
4. **Licence `MIT OR Apache-2.0`**, like the rest of adam-rs (the platform's crates were MIT).
5. **The API does not change**: group `agents.vymalo.com`, version `v1alpha1`, kinds `AgentService` and `AgentConfig`,
   and the finalizer `agents.vymalo.com/runtime` and labels under that group keep their names. The schema grew two
   optional fields (`spec.model.extraBody`, `spec.model.echoReasoning`, see *Parity*); the descriptions lost their
   references to the platform's section numbers.
6. **netcup hard-cuts over** to the new chart and image: breaking changes of the chart are fine.

## What moved, from where

| At `cfd03db` | Now |
|---|---|
| `crates/{api,domain,ports,controller,runtime-kubernetes,store-cnpg,store-secret,registry}` (`aap-*`) | `crates/adam-operator-{api,domain,ports,controller,runtime-kubernetes,store-cnpg,store-secret,registry}` |
| `bin/operator` (`aap-operator`: `run`, `crdgen`) | `bin/adam-operator` (`adam-operator`: `run`, `crdgen`) |
| `deploy/crds`, `deploy/operator`, `deploy/operator-crds` | the same paths; charts `adam-operator` and `adam-operator-crds` |
| `examples/` (`coder.yaml`, `chat.yaml`, `invalid/`) | `deploy/operator/examples/` |
| `docker/operator` (image `ghcr.io/vymalo/another-agentic-platform/operator`) | `docker/operator`, image `ghcr.io/vymalo/another-adam-rs/operator` |
| `tools/adam-parity` | `tools/adam-operator-parity`, rendering this repository's `deploy/coder` instead of a pinned clone |
| `.github/workflows/operator.yml` and `operator-image.yml` | `.github/workflows/operator.yml` |

The crates use adam-rs's pins (`kube` 4.2, `k8s-openapi` 0.28 on `v1_32`, TLS on `aws-lc-rs`: no second provider, and the
feature union does not change `adam-env-kubernetes`), `adam_error::Classify` for every error and `thiserror`
(`anyhow` only in the binary). `Cargo.lock` gained only the dependencies the operator needs (`kube-runtime`, `kube-derive`,
`serde_yaml_ng`, and `cel` for the CRD tests).

## Renamed

| Was | Is |
|---|---|
| crates `aap-api`, `-domain`, `-ports`, `-controller`, `-runtime-kubernetes`, `-store-cnpg`, `-store-secret`, `-registry` | `adam-operator-*` (same suffixes) |
| binary and package `aap-operator`, `/operator` in the image | `adam-operator`, `/adam-operator` |
| `AAP_CONCURRENCY`, `AAP_RESYNC_SECS`, `AAP_RESYNC_PENDING_SECS` | `ADAM_OPERATOR_CONCURRENCY`, `ADAM_OPERATOR_RESYNC_SECS`, `ADAM_OPERATOR_RESYNC_PENDING_SECS` |
| `AAP_TEST_KUBECONFIG`, `AAP_TEST_REQUIRE_CLUSTER`, `AAP_TEST_STUB_IMAGE`, `AAP_TEST_HOST_ADDR`, `AAP_TEST_NO_WORKLOADS` | the same with `ADAM_OPERATOR_TEST_` |
| metrics `aap_reconcile_total`, `aap_reconcile_errors_total`, `aap_status_patches_total`, `aap_service_state_changes_total`, `aap_runtime_signals_total`, `aap_services_deleted_total` | `adam_operator_*` (same suffixes) |
| field manager and `app.kubernetes.io/managed-by` value `aap-operator` | `adam-operator` |
| log targets `aap_*` (the module paths) | `adam_operator_*` |
| charts `aap-operator`, `aap-operator-crds`; their objects named `aap-operator` | `adam-operator`, `adam-operator-crds`; `adam-operator` |
| test namespaces `aap-test`, `aap-e2e-*`; e2e client label `aap-e2e/client`; kind clusters `aap-*` | `adam-operator-test`, `adam-operator-e2e-*`, `adam-operator-e2e/client`, `adam-*` |

Not renamed on purpose: `WATCH_NAMESPACE`, `HEALTH_ADDR`, `METRICS_ADDR`, `POD_NAME`, `REGISTRY_ADDR`,
`REGISTRY_TOKEN_FILE`, `REGISTRY_PUBLIC_URL` (they never had the prefix), the finalizer `agents.vymalo.com/runtime`, the
labels and annotations under `agents.vymalo.com/`, and the registry's `agent-registry/v1` contract.

## Parity

The goldens now come from this checkout's `deploy/coder` (`sh tools/adam-operator-parity/regen.sh --check`, run by the
workflow's `chart` job, so a change of the coder chart fails until the goldens and the domain agree). Gaps found since
`0391809`:

* **Closed: `MODEL_EXTRA_BODY` and `MODEL_ECHO_REASONING`** (`config.modelExtraBody`, `config.modelEchoReasoning` of the
  chart): `spec.model.extraBody` (a free-form object, never a Secret: the chart says it shows in the pod's environment) and
  `spec.model.echoReasoning` (`reasoning_content` or `reasoning`). `validate` refuses the members the process owns
  (`model`, `messages`, `tools`, `tool_choice`, `stream`), as the chart and the process do, and a golden case covers them.
* **Open: run pods** (below). The other variables added to the process since (`SCRATCH_CHECK_CYCLES`,
  `THREAD_TOOLS_MAX_CALL_SECS`, `MCP_ALLOW_URL_VARS`, `RUST_LOG`) are not set by the chart either; `spec.extraEnv` carries them.

## What is missing: run pods (ADR 0019)

The operator makes none of what the chart's `runPods` block makes, so an `AgentService` cannot give the coder a pod per
run. Exactly:

1. The coder container's variables `RUN_ENVIRONMENT=kubernetes`, `RUN_POD_TEMPLATE_FILE`, `RUN_POD_NAMESPACE` (the pod's
   namespace by field reference), `RUN_POD_INSTANCE`, `RUN_POD_CONTAINER`, `RUN_POD_READY_TIMEOUT_SECS`,
   `RUN_POD_IDLE_SECS`, `RUN_POD_WAIT_SECS`, and `WORKER_ID` from the pod name whenever run pods are on.
2. The pod template: a ConfigMap (`run-pods-configmap.yaml`), mounted read-only into the coder pod, with a checksum
   annotation so a change redeploys.
3. The coder pod's identity: a `ServiceAccount` whose token is mounted (the chart sets `automountServiceAccountToken: false`
   otherwise), a `Role` and `RoleBinding` that let it make, list and delete pods and `exec` in them, and smaller `resources`
   for the coder container while run pods are on.
4. Namespace objects the chart makes: a `ResourceQuota` scoped to a `PriorityClass`, a `NetworkPolicy` for the run pods, and
   a `PriorityClass` and a `ValidatingAdmissionPolicy` with its binding, which are **cluster-scoped**.
5. The API shape for all of it (an `AgentConfig` block), its validation (Kubernetes 1.30 or later for the admission policy)
   and its status.

Items 3 and 4 collide with the operator's design: it is namespaced, has no `ClusterRole` and no right on Secrets, and a
policy that lets one ServiceAccount make pods is exactly what an operator with those rights could abuse. Whether the
operator makes them, or the platform team applies them beside the `AgentService`, is an owner decision; it is not half
implemented here.

## What stays in the archived repository

The long-range design (the architecture sections, the decisions AD-001 to AD-024, `docs/mvp.md`, the open questions) and the
two contracts (`agent-registry/v1`, `release-channels/v1`) are not copied. The READMEs cite them as `§N`, `AD-NNN`, `Sn` and
`Mn`, and say so at their top; the links are permalinks to `cfd03db`. A guide, a CRD reference, the contracts and the skills
that explain this to a reader outside the repository come later.

## Consequences

* One repository changes when the env contract changes: `deploy/coder`, the process and the operator are tested together,
  and the operator's image is built and bumped by `operator.yml` as the coder's is by `coder.yml`.
* The workspace gains eight crates and a binary, and the dependency tree gains `kube-runtime`, `kube-derive`,
  `serde_yaml_ng` and (for the CRD tests only) `cel`: all within `deny.toml`'s licences (*verified 2026-10-07* from
  `cargo metadata`); advisories were not run (*unverified*: `cargo deny` is a CI job).
* The first push of the image creates a private GHCR package: the owner makes it public (`deploy/operator/README.md`).
* The CRD file gained two optional fields and lost the section references in its descriptions, so an installed CRD is
  upgraded by the new `adam-operator-crds`.
* The platform's e2e against the real coder image now pins this repository's own image (`sha-4363924`) and asserts the card of
  Adam, not of the old Coder.
