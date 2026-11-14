# 0046. The operator lives in the platform repository

Status: **Accepted** (2026-11-14), an owner decision. Supersedes [ADR 0029](0029-adam-rs-has-an-operator.md).

## Context

Two copies of the Kubernetes operator of `AgentService` and `AgentConfig` (`agents.vymalo.com/v1alpha1`) existed: this
repository's (ADR 0029) and the one in `vymalo/another-agentic-platform`, where it was written first. Two copies drift
(ADR 0029 records the first drift). The owner, 2026-11-14: "Destroy one if you see duplicates". No cluster runs either
operator today (the owner's statement of the same day: the deployment repository no longer has it and its cluster is down;
*unverified* otherwise).

## Decision

**adam-rs keeps no operator. The platform repository's is the only one.** Removed from this repository:

* the crates `crates/adam-operator-{api,controller,domain,ports,registry,runtime-kubernetes,store-cnpg,store-secret}` and the binary `bin/adam-operator`;
* the charts `deploy/operator` and `deploy/operator-crds`, the CRD file `deploy/crds`, the image `docker/operator`, and the parity tool `tools/adam-operator-parity`;
* the workflow `.github/workflows/operator.yml` (its image build, kind jobs and tag bump);
* the skill `adam-operator` and the guide `docs/guides/run-agents-with-the-operator.md`;
* the workspace dependencies only those crates used.

**Kept:** `deploy/coder` with its run-pod templates, `crates/adam-env-kubernetes`, `crates/adam-git-sandbox`,
`bin/adam-egress` and the `run-pods` CI job. The platform's operator reads `deploy/coder` at a pinned revision for its
parity goldens, so that chart stays the contract.

**Where the operator lives now:** `deploy/operator`, `deploy/operator-crds` and `crates/registry` of
https://github.com/vymalo/another-agentic-platform. Its CRDs accept every adam-rs resource, except for the three differences below, as of platform
commit `736f2dc` (the squash merge of its pull request 45 on its `main`, *verified 2026-11-14* from the merge); a test, `crates/api/tests/superset.rs`, compares them with this repository's CRD and lists
the differences (*verified 2026-11-14*, read in the platform's checkout). There are three:

1. `AgentConfig.spec.tools.mcpServers` accepts at most 32 servers.
2. `AgentConfig.spec.model.extraBody` accepts at most 64 members and 16 KiB of JSON.
3. `AgentService.spec.interfaces.a2a.enabled` defaults to `false`, not `true`.

**Not ported:** the chart's `cache.create`. An `AgentConfig` names a claim it does not own (platform
`crates/domain/tests/golden/README.md`, K1).

**The image** `ghcr.io/vymalo/another-adam-rs/operator` (tags `sha-<7>` and `latest`) stays in GHCR, frozen at its last
push, no longer built, bumped or maintained. Do not deploy it; use the platform's image. No Helm chart of this
repository's operator was ever pushed to a registry. The last source is at the parent of the commit that removed it:
`git log --diff-filter=D -- bin/adam-operator` finds it.

## Consequences

* A consumer that ran the operator from here moves to the platform's charts and image. `adam-upgrade` has a row for it.
* ADRs 0029, 0036, 0037 and 0040 keep their text; each has a dated note. ADRs that only record history are untouched.
* The platform's vendored copy of this repository's CRD stays a file at a fixed revision (`4bc6742`) and keeps working;
  its pointer is now historical (a follow-up in that repository).
* Skills other repositories vendor (`adam-a2a-extensions`, `adam-agent-folder`, `adam-coder-deploy`, `adam-upgrade`) now
  point to the platform for the operator. What `npx skills update` does about a skill removed from its source was not
  checked (*unverified*: the CLI is not installed here).
