# adam-operator-store-secret

> Moved into adam-rs from `vymalo/another-agentic-platform` at commit [`cfd03db`](https://github.com/vymalo/another-agentic-platform/tree/cfd03db836b39fd4ff81ac0275f2602781ab19c4) ([ADR 0029](../../docs/decisions/0029-adam-rs-has-an-operator.md)). `§N`, `AD-NNN`, `Sn` and `M0`..`M3` in this file cite that repository's design documents (`docs/architecture`, `docs/mvp.md`) as of that commit, which is archived; they are not links into this one.

`StoreProvisioner` for a **referenced Secret** ([§59a](https://github.com/vymalo/another-agentic-platform/blob/cfd03db836b39fd4ff81ac0275f2602781ab19c4/docs/architecture/10-control-plane-and-crds.md#secrets-and-databases),
AD-020, AD-024): the store of a service whose `spec.store.postgres.secretRef` names a Secret key that someone else
owns. Nothing is made. `ensure` checks that the reference is well formed (`StoreSpec::check`) and reports it:

```rust
let store = adam_operator_store_secret::SecretStore::new();
let status = store.ensure(&id, &spec).await?;   // StoreState::SecretReferenced, connection = the reference
```

| Call | Does |
|---|---|
| `capabilities()` | `{ cnpg: false }` |
| `ensure(id, spec)` with `StoreKind::Secret(r)` | `StoreStatus { state: SecretReferenced, connection: r }`; remembers the id |
| `ensure(id, spec)` with `StoreKind::Cnpg(_)` | `StoreError::Unsupported`: a cluster is [`store-cnpg`](../adam-operator-store-cnpg/README.md)'s (S6), and a build without that feature refuses it. The controller reports it as `StoreReady: False`, reason `CNPGNotInstalled` |
| `ensure` of a spec that fails `check` (an empty name or key) | `StoreError::InvalidSpec` |
| `release(id)` | `existed` is true when this provisioner ensured the id; `retained` is always false: a Secret someone else owns holds no data of ours, whatever the deletion policy says |

**It never reads a Secret, and it has no Kubernetes client at all** (the crate depends on `adam-operator-ports` and nothing
else). That is not an omission: the operator has **no RBAC on Secrets** (AD-024), because a `get` returns the values, and
Kubernetes has no verb that tells a Secret's keys without them. So *whether the Secret and its key exist* is not this
crate's to say. The kubelet says it: a pod that references a missing Secret or key stays in `CreateContainerConfigError`,
the runtime provider reports it as the issue `MissingSecret`, and the controller as `RuntimeReady: False`, reason
`MissingSecret` (§59a, "Status": "the kubelet's answer in the pod's status is how it learns"). `StoreState` has
`SecretReferenced` and not `SecretFound` for the same reason.

The S5 brief asked for a check that the Secret and its key exist. That contradicts AD-024 and §59a, and would need the
right the design withholds; the crate follows the design (see the controller's README, *Deviations*).

The provisioner keeps one thing in memory, the ids it ensured, so that releasing what it never saw says
`existed: false` as the contract wants. A restarted operator has forgotten them: the release of a service it did not
ensure since the restart says `existed: false`, which only changes the wording of one Event.

## Feature `testkit`

`impl StoreUnderTest for SecretStore` (its `materialised` is empty: nothing is written anywhere), so a composition
root can run `store_provisioner_conformance!` on it.

## Tests

`cargo test -p adam-operator-store-secret`: the seven cases of `store_provisioner_conformance!` (including "no secret value
materialises"), and the cases of this provisioner: the reference is reported under both deletion policies, a cluster
kind is `Unsupported`, two services are two stores. **They need no backend, so nothing skips and no cluster is involved**:
the provisioner has nothing to connect to. (The brief asked for the suite also to run against a real cluster in CI;
there is no cluster behaviour to run it against. The Secret's absence shows in the operator's end-to-end, in
`bin/adam-operator/tests/cluster.rs`: the case *a missing secret is a condition until the secret exists*.)
