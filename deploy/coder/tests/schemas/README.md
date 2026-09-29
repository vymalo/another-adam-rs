# CRD schemas for kubeconform

The chart renders two custom resources: a CloudNativePG `Cluster` and an `ExternalSecret`. The
`coder` workflow validates them with kubeconform against the JSON schemas in this directory.

Both files are byte-for-byte copies from
[datreeio/CRDs-catalog](https://github.com/datreeio/CRDs-catalog) at commit
`ad3b08c5045129d7bb1eeffd8e61719b2c8dd1e2` (fetched 2026-09-29):

| File | Resource |
|---|---|
| `postgresql.cnpg.io/cluster_v1.json` | `postgresql.cnpg.io/v1` `Cluster` |
| `external-secrets.io/externalsecret_v1.json` | `external-secrets.io/v1` `ExternalSecret` |

They are vendored because CI used to read the catalog live from `raw.githubusercontent.com`. When
that returned HTTP 500 on 2026-09-29, the image job was skipped and no image was published for that
commit. The core Kubernetes schemas are still downloaded, and the workflow retries that download.

To update, pick a newer catalog commit and fetch the same two paths. If the chart gains a custom
resource, add its schema here, because kubeconform fails on a kind that has no schema.

```sh
C=<catalog commit>
for f in postgresql.cnpg.io/cluster_v1.json external-secrets.io/externalsecret_v1.json; do
  curl -fsSL -o "deploy/coder/tests/schemas/$f" "https://raw.githubusercontent.com/datreeio/CRDs-catalog/$C/$f"
done
```
