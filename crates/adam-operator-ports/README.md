# adam-operator-ports

> Moved into adam-rs from `vymalo/another-agentic-platform` at commit [`cfd03db`](https://github.com/vymalo/another-agentic-platform/tree/cfd03db836b39fd4ff81ac0275f2602781ab19c4) ([ADR 0029](../../docs/decisions/0029-adam-rs-has-an-operator.md)). `§N`, `AD-NNN`, `Sn` and `M0`..`M3` in this file cite that repository's design documents (`docs/architecture`, `docs/mvp.md`) as of that commit, which is archived; they are not links into this one.

The provider seams of the v0 operator ([§59a](https://github.com/vymalo/another-agentic-platform/blob/cfd03db836b39fd4ff81ac0275f2602781ab19c4/docs/architecture/10-control-plane-and-crds.md#59a-operator-v0-adam-rs-agents),
[§22](https://github.com/vymalo/another-agentic-platform/blob/cfd03db836b39fd4ff81ac0275f2602781ab19c4/docs/architecture/05-runtime.md#22-runtimeprovider), AD-020): three traits and the neutral
types that cross them.

| Trait | Does | Implemented by (later slices) |
|---|---|---|
| `RuntimeProvider` | makes an agent's compute exist, says how it is doing, tells the controller what changed | `runtime-kubernetes` (S4); `MemoryRuntime` here |
| `StoreProvisioner` | makes the agent's run ledger exist: a referenced Secret, or an operator-owned CloudNativePG cluster | `store-secret`, `store-cnpg` (S5, S6: `CnpgStore` serves both kinds); `MemoryStore` here |
| `AgentDirectory` | lists the agents the platform runs, for the registry | the controller's reflector (S5), read by `adam-operator-registry` (S7); `MemoryDirectory` here |

**No Kubernetes type, no driver type and no secret value appears in any signature.** A secret has no
field anywhere in this crate: it travels as a `SecretRef { name, key }` and a provider hands it to the
process without reading it (AD-024).

Where it sits: `adam-operator-ports` depends on nothing of the workspace. `adam-operator-domain` depends on it for the
output of `resolve` (§59a: "the mapping of `adam-coder` / `adam-agent` onto `adam_operator_ports::RuntimeSpec`"), and
`adam-operator-api` knows nothing of it; the controller (S5) is generic over `<R: RuntimeProvider, S: StoreProvisioner>`.

## The traits

All methods are **idempotent**; the id is the only identity.

```rust
trait RuntimeProvider: Send + Sync {
    fn name(&self) -> &'static str;                 // status.runtime.provider
    fn capabilities(&self) -> Capabilities;         // { suspend }
    async fn ensure(&self, id: &RuntimeId, spec: &RuntimeSpec) -> Result<RuntimeStatus, RuntimeError>;
    async fn suspend(&self, id: &RuntimeId) -> Result<RuntimeStatus, RuntimeError>;
    async fn delete(&self, id: &RuntimeId) -> Result<DeleteOutcome, RuntimeError>;
    async fn status(&self, id: &RuntimeId) -> Result<RuntimeStatus, RuntimeError>;
    async fn endpoint(&self, id: &RuntimeId, surface: Surface) -> Result<Endpoint, RuntimeError>;
    fn watch(&self) -> BoxStream<'static, RuntimeId>;
}
trait StoreProvisioner: Send + Sync {
    fn capabilities(&self) -> StoreCapabilities;    // { cnpg }
    async fn ensure(&self, id: &StoreId, spec: &StoreSpec) -> Result<StoreStatus, StoreError>;
    async fn release(&self, id: &StoreId) -> Result<ReleaseOutcome, StoreError>;
}
trait AgentDirectory: Send + Sync {
    async fn list(&self) -> Result<Vec<DirectoryEntry>, DirectoryError>;
    async fn get(&self, scope: &str, name: &str) -> Result<Option<DirectoryEntry>, DirectoryError>;
}
```

(`async fn` is shorthand: the declared form is `fn … -> impl Future<Output = …> + Send`, see below.)

### Native async, not `async-trait`

The methods are native `async fn` in the trait, declared as `-> impl Future<Output = …> + Send`.
Reasons: no heap allocation per call and no macro; the `Send` bound is what lets a controller `spawn`
a reconcile that calls them; an implementation writes plain `async fn`. The price is that **the traits
are not object safe** (no `dyn RuntimeProvider`). §59a makes the controller generic and the provider a
build-time choice (AD-020), so nothing needs `dyn`. If a composition root ever wants one, a small
`DynRuntimeProvider` adapter with boxed futures can be added next to the trait without touching an
implementation.

### What the methods mean where §22 and §59a say little

* **`ensure`** creates, changes or wakes (there is no `activate`, AD-023). It first runs
  `RuntimeSpec::check`, so every provider rejects the same malformed spec with `InvalidSpec`.
* **A blocked runtime is not an error.** A name held by an object that is not ours (the adoption
  guard) is an `Ok(RuntimeStatus)` with an issue `NameConflict` and nothing changed; a missing Secret, a
  crash loop or an image that cannot be pulled are issues of the status too. `RuntimeError` is for calls
  that failed (`Unavailable`), not for a runtime that is unwell.
* **`delete(id)` takes no spec.** The deletion policy of the last `ensure` is on the objects the
  provider made, so a restarted controller can still delete (§59a: "the policy reaches the provider … in
  its neutral spec type"). The outcome says which persistent volumes stayed (`Retain`).
* **`status` of what does not exist is `Phase::Absent`**, not `NotFound`.
* **`RuntimeStatus.replicas`** are the ready replicas of the workloads that step runs (`Role::All` and
  `Role::Worker`), so `split` does not count the front.
* **`watch` is a hint** ("a notification is never the truth"): ids of runtimes whose state changed since
  the stream was taken, at most once, possibly also for runtimes the caller does not care about. The
  controller still reconciles on a timer and reads `status`.
* **`StoreProvisioner::ensure`** returns the Secret key that holds the connection string
  (`StoreStatus.connection`) and a `StoreState` (`SecretReferenced`, `ClusterReady`, `ClusterNotReady`).
  A cluster kind on a backend without CloudNativePG is `StoreError::NotInstalled` (the
  `CNPGNotInstalled` condition), not a status. `cnpg_connection("coder")` is `coder-db-app` key `uri`:
  the one definition of that name, used by `adam-operator-domain` for `DATABASE_URL` and by every provisioner.
* **`AgentDirectory::list`** returns every service, listed or not, ordered by scope and name, with the
  flags the registry needs (`a2a_enabled`, `blocked`, the card URL); `DirectoryEntry::listed()` is the
  rule of §59a ("A2A enabled, not `Blocked`") in one place. Before its first sync a directory answers
  `NotReady`, never an empty list that looks true.

### Errors

`thiserror` enums (`RuntimeError`, `StoreError`, `DirectoryError`), each with a class
(`Classify::class() -> ErrorClass`: `Transient`, `Conflict`, `Invalid`, `NotFound`, `Unsupported`,
`Internal`). A caller decides from the class, never from the variant, the way the sibling repository
`another-adam-rs` does with `adam_error::Classify`; the trait is small enough that it is defined here
instead of depending on that repository.

## The neutral types

| Type | What |
|---|---|
| `RuntimeId`, `StoreId` | scope (a namespace) and name of the service. The same on every call |
| `OwnerHandle` | **opaque** token the controller got from the object and a provider understands (owner references). Never read by the controller, never part of a digest |
| `SecretRef { name, key }` | the only form of a secret |
| `RuntimeSpec` | what to make: `workloads`, `file_sets`, `network`, `suspend`, `deletion`, `owner`, and the `digest` the pods are stamped with |
| `Workload` | one set of identical pods: `role`, `replicas`, `stable_identity`, an agent `Container`, native `sidecars`, `volumes`, `security`, the grace period and a disruption budget |
| `Container`, `EnvVar`, `Mount`, `Probe`, `Resources` | what runs |
| `EnvValue::{Literal, Secret(SecretRef), PodName}` | where a variable comes from. Closed: a new source fails to compile in every provider |
| `VolumeSpec`, `VolumeSource::{Persistent, Files, ExternalFiles, SecretFile}`, `Sharing::{PerReplica, Shared}` | volumes; persistent ones are data |
| `FileSet` | files materialised by name (ConfigMaps); paths may contain `/` and a provider maps them |
| `Network`, `Peer`, `Selector`, `IpBlock` | the Service port, which workload it selects, and the peers of the ingress rule (the neutral form of a `NetworkPolicyPeer`) |
| `Role::{All, ControlPlane, Worker}` | adam's `ROLE`. Closed |
| `RuntimeStatus { phase, replicas, issues }`, `Phase`, `Issue { role, reason, message }`, `IssueReason` | the §59a table: `ConfigRejected`, `DependencyUnavailable`, `MissingSecret { name }`, `ImagePull`, `CrashLoop`, `NameConflict`. Closed |
| `Capabilities`, `Surface`, `Endpoint`, `DeleteOutcome` | |
| `StoreSpec`, `StoreKind::{Secret, Cnpg}`, `CnpgSpec`, `StoreStatus`, `StoreState`, `StoreCapabilities`, `ReleaseOutcome` | the store |
| `DirectoryEntry` | the directory entry type for the registry |
| `DeletionPolicy::{Retain, Delete}` | the neutral form of `adam_operator_api::DeletionPolicy` |

Choices worth knowing, because §59a does not say:

* **`stable_identity`.** §59a picks a StatefulSet "when a `perReplica` persistent volume exists". A
  placement of `affinity` pins runs to a worker by `WORKER_ID` = the pod name, and then needs the name to
  survive a restart, but its volume is one shared claim. `stable_identity` says it in neutral terms
  (true for a per-replica volume *or* a pod-name variable) and a provider that has a StatefulSet picks it
  on that.
* **The digest covers what the pods run** and not scale, ownership, the deletion policy or the network
  policy (see `adam-operator-domain`): a bigger `scaling.workers` must not roll the running pods. A provider only
  copies `RuntimeSpec::digest` onto the pods.
* **An empty `network.allow_from` makes no policy**, it does not deny everything: the bearer token is
  the gate, and an agent nobody can reach is a worse default than one that needs the token.
* **`Capabilities` has one flag**, `suspend`, because it is the only optional verb. Things that were
  tempting (`sidecars`, `shared_volumes`) are not optional for v0 and would only be flags no case could
  test.

## Feature `testkit`

Off by default (the controller and the providers depend on the traits only). With it:

* **Conformance macros**, in the style of `adam-store-testkit`:
  `runtime_provider_conformance!(make)` (14 cases), `store_provisioner_conformance!(make)` (7) and
  `agent_directory_conformance!(make)` (5). `make` is a path to `async fn() -> Option<T>`; `None` skips
  the suite, and `ADAM_OPERATOR_TEST_REQUIRE_BACKEND=1` turns the skip into a failure (CI sets it for the suites that
  need a cluster). The macros expand to `#[tokio::test]`, so the crate that uses them needs `tokio` with
  `macros` and `rt`.
* The type under test also implements `RuntimeUnderTest`, `StoreUnderTest` or `DirectoryUnderTest`: a
  `materialised(id)` that returns **every plain-text value the provider wrote to its backend** (literal
  variables, commands, file contents, labels), and for a directory, `put` and `remove` into its source.
  The case **"no secret value materialises"** puts a sentinel in the names of the Secrets of a spec and
  fails if it is anywhere in that text or in an issue message. It is as honest as `materialised`: a
  Kubernetes provider feeds it the real objects it applied, minus `secretKeyRef`.
* `sample_spec(id)`: a spec that uses every kind of thing a provider must handle (a sidecar, literal,
  secret and pod-name variables, a per-replica volume, a file set, a secret file, probes). Providers reuse
  it in their own tests.
* **`memory::{MemoryRuntime, MemoryStore, MemoryDirectory}`**: in-memory implementations that pass their
  own suites (`tests/memory.rs`, with and without `suspend`, with and without CloudNativePG) and serve the
  controller's unit tests: `set_foreign` (the adoption guard), `force_status` (a crash loop),
  `set_unavailable`, `set_cnpg_installed`, `set_cluster_ready`, `set_ready`.

```rust
async fn make() -> Option<MemoryRuntime> {
    Some(MemoryRuntime::new())
}
adam_operator_ports::runtime_provider_conformance!(make);
```

## Tests

`cargo test -p adam-operator-ports`:

* `tests/memory.rs`: the five suites against the in-memory implementations (47 cases).
* `tests/contract.rs`: `RuntimeSpec::check` names each broken invariant, a spec round-trips through
  JSON, every error has the class that decides retrying, the suites **fail** what they must (a provider
  that materialises a secret, a `watch` that never speaks), the adoption guard, a forced status, an
  unreachable backend, a missing CloudNativePG API, `DirectoryEntry::listed`, a directory that has not
  synced.

The in-memory implementations are instant: they cannot show that a real provider's `watch` is timely
or that its `status` reflects pod conditions. Those belong to S4's envtest harness, which runs the same
macros.
