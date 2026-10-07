# CRD schemas for kubeconform

The chart renders one custom resource, an `ExternalSecret` (the registry's token, with `externalSecrets.enabled`). The
`operator` workflow validates the render with kubeconform against the JSON schema in this directory.

| File | Resource |
|---|---|
| `external-secrets.io/externalsecret_v1.json` | `external-secrets.io/v1` `ExternalSecret` |

The file is a byte-for-byte copy of the one in [vymalo/another-adam-rs](https://github.com/vymalo/another-adam-rs)
`deploy/coder/tests/schemas` and in [vymalo/another-agentic-system](https://github.com/vymalo/another-agentic-system)
`deploy/chart/tests/schemas`, which are copies from [datreeio/CRDs-catalog](https://github.com/datreeio/CRDs-catalog) at commit
`ad3b08c5045129d7bb1eeffd8e61719b2c8dd1e2` (fetched 2026-09-29, per the adam-rs directory's README; copied here 2026-10-05).

It is vendored because a live catalog that answers HTTP 500 once skipped a whole image job in adam-rs's CI. The core Kubernetes
schemas are still downloaded, and the workflow retries that download.

To update, pick a newer catalog commit and fetch the same path. If the chart gains another custom resource, add its schema
here, because kubeconform fails on a kind that has no schema.

```sh
C=<catalog commit>
curl -fsSL -o deploy/operator/tests/schemas/external-secrets.io/externalsecret_v1.json \
  "https://raw.githubusercontent.com/datreeio/CRDs-catalog/$C/external-secrets.io/externalsecret_v1.json"
```
