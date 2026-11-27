# 0080. Helm 4 and Kubernetes 1.37, a hard cutover

Status: **Accepted** (2026-11-27). The owner decided on 2026-11-27: *"We'll support helm 4 only from now on. Hard
cutover, move everything from helm 3. Support starts at k8s 1.37 too."*

## Context

The chart `deploy/coder` was rendered with Helm 3 (`HELM_VERSION: v3.19.0` in `.github/workflows/coder.yml`) and
declared `kubeVersion: ">=1.27.0-0"`. CI applied it to a kind cluster on `kindest/node:v1.35.8` and validated the
renders with kubeconform against Kubernetes 1.31.0. Nothing in the repository installs with `--wait`, `--atomic` or
`--timeout`; the whole Helm surface is `helm lint`, `helm template` (with `--set`, `--set-string`, `--set-json`,
`-f`, `--namespace`, `--show-only`, `--debug` and `--kube-version`) and the documented `helm upgrade --install`.

Helm 4.3.2 is the release this decision pins (*verified 2026-11-27*: `helm version` reports `v4.3.2`,
`KubeClientVersion: v1.37`). It changes what an install does: server-side apply by default (`--server-side`, default
true), `--wait` is a strategy (`watcher`, kstatus, when the flag is given alone; `hookOnly` when it is omitted;
`legacy` for the Helm 3 behaviour), and `--atomic` is deprecated in favour of `--rollback-on-failure`. `helm lint`
and `helm template` keep the flags this repository uses (*verified 2026-11-27* from the 4.3.2 `--help`).

## Decision

1. **Helm 4 only, a hard cutover.** CI installs `v4.3.2` through `azure/setup-helm@v5`; the repository keeps no Helm
   3 path, no version-conditional template and no dual support. Rendering `deploy/coder` with a Helm 3 binary is not
   a supported input.
2. **Kubernetes 1.37 is the floor.** `deploy/coder/Chart.yaml` declares `kubeVersion: ">=1.37.0-0"`;
   `helm template --kube-version 1.36.0` fails and `--kube-version 1.37.0` succeeds, both asserted by
   `deploy/coder/tests/render-check.sh`. The `runPods` guard in `templates/_validate.tpl` names the same floor. CI
   validates the renders with kubeconform against 1.37.0 and runs the run-pods job on `kindest/node:v1.37.0` by
   digest.
3. **One golden change, and it is whitespace.** Helm 4 writes a blank line before each document separator, so
   `tests/golden/{combined,run-pods,a2a-split,cache}.yaml` are re-rendered from the chart: the diff is one blank line
   per document boundary and nothing else. The chart version stays 0.5.0, so no rendered label moves. The platform
   renders this chart for its parity goldens (`tools/adam-parity`); its re-render changes by the same whitespace
   and nothing else.
4. **The Helm 4 behaviour is the documented one.** The deploy docs say `helm upgrade --install`; from Helm 4 that is
   a server-side apply, and a StatefulSet field the API server refuses is still refused. No value of this chart asks
   for `--wait`, `--atomic` or `--timeout`, so the kstatus wait and the `--atomic` rename cost nothing here; the
   `adam-coder-deploy` skill names them for whoever adds one.

## Consequences

- **CloudNativePG is the cost.** `database.enabled: true` (the default) renders a CNPG `Cluster`, and CloudNativePG
  1.30.x — the version ADR 0053 pins — supports Kubernetes 1.34, 1.35 and 1.36, and lists 1.37 as *tested, but not
  supported* (*verified 2026-11-27*: https://cloudnative-pg.io/docs/1.30/supported_releases). A 1.37 cluster with the
  default database is therefore outside the operator's supported matrix until CloudNativePG 1.31 or later (1.31.0 is
  planned for December 2026). `database.enabled: false` with an existing Postgres avoids it.
- **kubeconform follows the floor.** `KUBERNETES_VERSION` moves from 1.31.0 to 1.37.0. kubeconform v0.7.0 has the
  `v1.37.0-standalone-strict` schemas (*verified 2026-11-27*: the default render, the run-pods render, the a2a-split
  render and the cache render validate — 52 resources, 0 invalid).
- **The kind node image moves, and its network behaviour is unverified until CI.** `kindest/node:v1.37.0`'s index
  digest is `sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5` (*verified 2026-11-27* from the
  Docker Hub tag API). kindnet enforcing `NetworkPolicy` was verified on v1.35.8 (run 40435825771, 2026-10-27); the
  `run-pods` job is the evidence for v1.37.0.
- **The floor is the chart's, not every feature's.** The chart README still says where a feature needs 1.29 or 1.30
  (native sidecars, the admission API); those remain true facts, but no supported deployment can be below 1.37.
- **ANR-831** (the wider upgrade card) no longer has to carry Helm: this card lands the Helm and Kubernetes floor
  first.
