# Parity goldens

> Moved into adam-rs from `vymalo/another-agentic-platform` at commit [`cfd03db`](https://github.com/vymalo/another-agentic-platform/tree/cfd03db836b39fd4ff81ac0275f2602781ab19c4) ([ADR 0029](../../../../docs/decisions/0029-adam-rs-has-an-operator.md)). `§N`, `AD-NNN`, `Sn` and `M0`..`M3` in this file cite that repository's design documents (`docs/architecture`, `docs/mvp.md`) as of that commit, which is archived; they are not links into this one.

What the chart [`deploy/coder`](../../../../deploy/coder) of **this repository** renders, projected onto the shape
`tests/parity.rs` reads, so that `adam-operator-domain` is held equal to it.

| File | What | Values |
|---|---|---|
| `coder.json` | the netcup coder: topology combined, a GitHub App by owners, a per-replica `work` volume, two extra MCP servers, the sidecar | [`tools/adam-operator-parity/cases/coder.yaml`](../../../../tools/adam-operator-parity/cases/coder.yaml), equivalent to `deploy/operator/examples/coder.yaml` |
| `coder-split.json` | the same, split: a control-plane front (2 replicas, with a budget) and 2 isolated workers | `cases/coder-split.yaml` |
| `coder-affinity-token.json` | two affinity workers on one shared ReadWriteMany claim, a GitHub token, `createRepoOwners`, a GitHub Enterprise sidecar on port 9090, a literal gateway URL, `extraEnv`, no extra MCP servers | `cases/coder-affinity-token.yaml` |
| `chat-env.json` | `adam-agent` for `deploy/operator/examples/chat.yaml`. **Written by hand, not rendered**: the chart renders `adam-coder` only. Derived from `bin/adam-agent/README.md` and `docker/coder/Dockerfile`; *verified 2026-10-05 by reading them, not by running the binary* | |

The chart is in this repository, so the revision of a golden is the one that holds the file. Each rendered golden records
`source`: the chart, the values file, the release, the namespace and the helm version.

## Regenerating them

They need `helm` 3.x (`HELM=/path/to/helm` names one off `PATH`) and `python3` with PyYAML. No cluster, no network:

```sh
sh tools/adam-operator-parity/regen.sh          # rewrite the goldens
sh tools/adam-operator-parity/regen.sh --check  # fail if the checked-in ones are stale
```

[`regen.sh`](../../../../tools/adam-operator-parity/regen.sh) renders the chart for each file in
[`tools/adam-operator-parity/cases`](../../../../tools/adam-operator-parity/cases), and
[`extract.py`](../../../../tools/adam-operator-parity/extract.py) keeps only what the operator promises (pods,
env, mounts, probes, resources, volumes, claims, the MCP file, budgets, the Service selector).
**CI runs `--check`** (`.github/workflows/operator.yml`, job `chart`) and the tests against the checked-in files
(`ci.yml`). A change of `deploy/coder` that changes what the pods run fails `--check` until the goldens are regenerated;
read the diff: **it is the change of the env contract**. Then fix `adam-operator-domain` until
`cargo test -p adam-operator-domain` passes, and say in the commit what changed. Every case pins `image.tag`, so the
tag bumps of the coder workflow do not stale the goldens.

## What differs on purpose

`tests/parity.rs` normalises exactly these, and `the_normalisations_are_the_documented_ones` fails if it
starts to hide anything else, so this list is kept honest by a test:

1. **`PUBLIC_URL`'s default host.** The chart says `http://<name>.<ns>.svc.cluster.local:8080/`; §59a (and its
   status example) says `http://<name>.<ns>.svc:8080/`. Both resolve in a cluster; the operator follows §59a.
2. **The image's digest.** The chart pins `repository:tag` only; the operator's examples pin
   `repository:tag@sha256:…` ("tag and digest", as the comments of the examples say). Compared before the `@`.

And these are not compared, because they are structure or the chart's own business:

* The container is named `coder` by the chart and `agent` by the operator; the projection keys it as `agent`.
* **The chart's conveniences the operator does not have in v0:** the `ExternalSecret` (secrets are references
  to Secrets someone else makes, AD-024), the CloudNativePG `Cluster` (the operator's own, later: S6),
  `imagePullSecrets`, `podLabels`/`podAnnotations`, `nodeSelector`, `tolerations`, `affinity`, `config.role`
  (an `AgentService` has `topology`), `modelBaseUrlFromSecret`'s placeholder logic (a `baseUrl` is a `value` or a
  `secretRef`), `workspace.sharedVolume.existingClaim` (the volume's size and class come from `AgentConfig`),
  and per-server `tools` allow-lists of the extra MCP servers.
* **What the operator does that the chart cannot:** a Secret name and key of any spelling (the chart's are
  fixed: `MODEL_API_KEY`, `GITHUB_TOKEN`, …), several headers and any number of extra MCP servers (the chart has
  `websearch` and `context7`, one header each), `MCP_ALLOW_INSECURE=true` whenever `tools.allowInsecureHttp` is
  (the chart sets it only for a plain-http extra server; `examples/chat.yaml` has none and still needs it for the
  orchestrator's thread tools), and the whole of `adam-agent` (a folder, `ADAM_AGENT_DIR`, the command).
* **Validation messages** differ (the chart's are `fail` strings, the operator's are `ConfigIssue`s); the
  *mistakes* are the same.

## Differences from §59a's table that the goldens revealed

None in the variable names, defaults or sources: every variable of the three renders is set by `resolve` with
the same source (literal, Secret reference or pod name) and, but for the two above, the same value. The
differences found are in §59a's *structure* and are listed in the READMEs of `adam-operator-ports` and `adam-operator-domain`
(a StatefulSet is needed by `affinity` too; the digest's scope), not in the contract.
