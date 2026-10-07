# adam-operator-api

> Moved into adam-rs from `vymalo/another-agentic-platform` at commit [`cfd03db`](https://github.com/vymalo/another-agentic-platform/tree/cfd03db836b39fd4ff81ac0275f2602781ab19c4) ([ADR 0029](../../docs/decisions/0029-adam-rs-has-an-operator.md)). `§N`, `AD-NNN`, `Sn` and `M0`..`M3` in this file cite that repository's design documents (`docs/architecture`, `docs/mvp.md`) as of that commit, which is archived; they are not links into this one.

The CRD types of the v0 operator, and nothing else: `AgentService` and `AgentConfig`, group
`agents.vymalo.com`, version `v1alpha1`, namespaced. The specification is
[§59a](https://github.com/vymalo/another-agentic-platform/blob/cfd03db836b39fd4ff81ac0275f2602781ab19c4/docs/architecture/10-control-plane-and-crds.md#59a-operator-v0-adam-rs-agents)
("The v0 CRDs", "Validation"); this crate follows it field for field, in camelCase.

No client, no controller, no I/O: the crate derives the types with `kube` (`CustomResource`,
`KubeSchema`), `schemars` and `serde`, and returns the definitions.

```rust
let crds: Vec<CustomResourceDefinition> = adam_operator_api::crds(); // AgentConfig, then AgentService
```

`adam-operator crdgen` ([`bin/adam-operator`](../../bin/adam-operator/README.md)) prints them as YAML into
[`deploy/crds/agents.vymalo.com.yaml`](../../deploy/crds/agents.vymalo.com.yaml).

## Public API

| Item | What |
|---|---|
| `AgentService`, `AgentServiceSpec`, `AgentServiceStatus` | The service: `configRef`, `interfaces`, `scaling`, `suspend`, `store`, `access`, `registry`, `deletionPolicy`, and the status the controller writes |
| `AgentConfig`, `AgentConfigSpec`, `AgentConfigStatus` | The config: `harness`, `model`, `tools`, `environment`, `security`, `extraEnv` |
| `SecretKeyRef { name, key }` | **A secret is never a value (AD-024)**: a field that needs one names a Secret and a key. A test fails if a string field is named like a secret |
| `condition_type`, `reason` | The condition types and reasons of §59a, "Status", as constants, so the controller and its tests do not spell them twice |
| `crds()` | The two `CustomResourceDefinition`s, in a fixed order |
| `GROUP`, `VERSION` | `agents.vymalo.com`, `v1alpha1` |

`spec.model` also carries `extraBody` (a free-form object, `MODEL_EXTRA_BODY`) and `echoReasoning`
(`reasoning_content` or `reasoning`, `MODEL_ECHO_REASONING`): not secrets, both optional.

Optional settings of `adam-coder` are `Option`s: a field left out is not set, and the binary's own
default applies (the operator does not copy adam's defaults). The defaults the schema does apply
are the platform's own: `scaling` (`combined`, 1 worker), `suspend: false`, `deletionPolicy:
Retain`, `interfaces` (nothing exposed), `githubMcp` (`sidecar: true`, `port: 8082`).

## CEL rules (`x-kubernetes-validations`)

Every rule §59a lists for the CRD is a CEL rule on the schema, so an API server refuses the
object. Each has an invalid example in [`examples/invalid`](../../deploy/operator/examples/invalid), whose
`# expect:` line is the message the API server must answer with.

| Rule | Where |
|---|---|
| `interfaces.responses.enabled` and `interfaces.mcp.enabled` are `false` | on the field |
| exactly one of `store.postgres.secretRef` and `store.postgres.cnpg` | `PostgresStore` |
| exactly one of `github.app` and `github.token` | `Github` |
| exactly one of `app.installationId` and `app.owners` | `GithubApp` |
| exactly one of `agent.folder.files` and `agent.folder.configMapRef` | `Folder` |
| exactly one of `agent.folder` and `agent.embedded` | `AgentSource` |
| `binary: adam-coder` needs `embedded` and the `coder` block | `Adam` |
| `binary: adam-agent` needs `folder` and no `coder` block | `Adam` |
| `scaling.front` only with `topology: split` | `Scaling` |
| exactly one of `model.baseUrl.value` and `model.baseUrl.secretRef` | `BaseUrl` (**added**: same shape, not in the list of §59a) |

Nothing §59a lists as a CRD rule was left out, so this crate has no `validate()` function.
What §59a assigns to the reconciler (`adam-operator-domain::validate`, slice S2) is **not** here: the
placement values and their volume rules, the MCP server URL and header rules, `extraEnv` names,
the `githubMcp` port beyond its 1 to 65535 range (the range is also in the schema), a folder over
1 MiB.

## Tests

`cargo test -p adam-operator-api`:

- `tests/crds.rs`: the identity of the two CRDs, every rule above is in the generated schema, the
  schema has no rule the tests do not know, and no string field is named like a secret.
- `tests/examples.rs`: the examples round-trip through the types and **lose nothing** (a misspelt
  field would be dropped silently by serde); the valid ones pass every CEL rule; each invalid one
  is refused with the message it states; every rule has an invalid example.

The CEL rules are evaluated with `kube-cel` (the `cel` feature of `kube`, a dev-dependency), a
client-side implementation. **It is a proxy for an API server, not one**, and it does not check
the OpenAPI schema (types, `required`, `enum`). The `kind` job of
[`.github/workflows/operator.yml`](../../.github/workflows/operator.yml) applies the CRDs, the
examples and the invalid examples to a real API server.

The golden of the generated YAML is [`deploy/crds/agents.vymalo.com.yaml`](../../deploy/crds/agents.vymalo.com.yaml),
tested by `adam-operator`.

## Versions

`kube` 4.0.0 (`derive`; the lock pins it, `cargo update` would take 4.2.0), `k8s-openapi` 0.28.0
(`v1_32`, `schemars`), `schemars` 1.2.2. Resource requirements, the label selectors of `allowFrom`
and `Quantity` are `k8s-openapi` types, so the schema embeds their upstream descriptions.
