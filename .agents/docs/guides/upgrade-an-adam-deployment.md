# Upgrade an adam deployment: evidence and commands

For people: [Upgrade an adam deployment](../../../docs/guides/upgrade-an-adam-deployment.md). That page decides; this one gives the
commands and the evidence. Checked at `257175c` on 2026-11-26. The skill for a repository that consumes adam-rs
([`adam-upgrade`](../../skills/adam-upgrade/SKILL.md)) is the long form of steps 1 to 3; a deployment needs the chart parts below.

## Procedure

1. **Name A.** The tag running now:

   ```sh
   kubectl -n <ns> get statefulset <fullname> -o jsonpath='{.spec.template.spec.containers[*].image}'
   ```

   The StatefulSet is the worker (the whole process with `topology: combined`); `<fullname>-front` is the front with `topology: split`.
2. **Name B.** A release `X.Y.Z` (read its digest with `sh tools/release/registry-digest.sh ghcr.io/vymalo/another-adam-rs/coder X.Y.Z`,
   [releases](releases.md#read-a-releases-digests)) or a commit of `main` whose image exists. Anonymous pull works for the package:

   ```sh
   TOKEN=$(curl -s "https://ghcr.io/token?scope=repository:vymalo/another-adam-rs/coder:pull" | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
   A='Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json'
   curl -sI -H "Authorization: Bearer $TOKEN" -H "$A" https://ghcr.io/v2/vymalo/another-adam-rs/coder/manifests/sha-<7> | grep -i '^docker-content-digest'
   ```

   `200` and a digest mean the tag exists. The image's revision label is the full commit: read the manifest's `config.digest`, fetch that blob, compare
   `.config.Labels["org.opencontainers.image.revision"]` with B (`adam-upgrade`, step 7). The chart renders `<image.repository>:<image.tag>`
   (`deploy/coder/templates/statefulset.yaml:83`); there is no digest value. *Tried 2026-11-27* (ADR 0084): `--set image.tag=X.Y.Z@sha256:<64 hex>` renders with the defaults and with
   `runPods.enabled=true` (every coder image, the run-pod template and the admission policy carry the pair), and the default render validates with kubeconform 1.37.0.
3. **List what changed**, in a clone of this repository:

   ```sh
   git log --oneline A..B -- crates bin dev docs/decisions deploy docker
   git log --format='%h %s' A..B | grep '!:'
   git diff A..B -- deploy/coder/README.md docs/guides/deploy-the-coder.md docs/reference/environment.md
   ```

   `chore(deploy): bump coder to sha-<7>` commits are image bumps, not changes.
4. **Render before the roll:** `helm template <release> deploy/coder -n <ns> -f my-values.yaml`; `sh deploy/coder/tests/render-check.sh` asserts the chart's own guarantees.
5. **Roll:** `helm upgrade --install <release> deploy/coder -n <ns> -f my-values.yaml`.
6. **Verify:** `GET <PUBLIC_URL>.well-known/agent-card.json`, `version` is `<crate version>+<first 7>` (ADR 0028). `EXPECT_REVISION` is what CI's smoke tests compare it with.

## Evidence

| Claim on the page for people | Where it is true |
|---|---|
| the image is pushed after the smoke tests, then `image.tag` is bumped | `.github/workflows/coder.yml`; `docs/guides/deploy-the-coder.md` "How a change reaches the chart" |
| the chart only knows `image.tag` | `deploy/coder/values.yaml:7-12` |
| every role migrates at start, after a connection check | `crates/adam-service/src/serve.rs:103-115` (`ChosenStore::ready`) and `:889-891` (its call); SQLite and MongoDB migrate when chosen, `:678` and `:705` |
| a failed migration exits by its class (69 for an unreachable store) | `crates/adam-service/src/exit.rs:91-92` (`Connect` and `Migrate` add no code; the cause next in the chain decides, by class), test at `:187` |
| PostgreSQL schema version 6, MongoDB 5, SQLite 1 | `crates/adam-store-postgres/src/lib.rs:89`, `crates/adam-store-mongodb/src/lib.rs:113`, `crates/adam-store-sqlite/src/lib.rs:69` |
| the migration returns at once on a current schema, takes no table lock then | `crates/adam-store-postgres/src/lib.rs:255-284` |
| it serialises with an advisory lock | `crates/adam-store-postgres/src/lib.rs:309` |
| the version is raised and never lowered | `crates/adam-store-postgres/src/lib.rs:765-770`; MongoDB `crates/adam-store-mongodb/src/lib.rs:882` (`$lt`) |
| SQLite refuses a newer file | `crates/adam-store-sqlite/src/lib.rs:488-496` |
| a v4 process fails a run the contract converted | `docs/guides/deploy-the-coder.md` "The contract release", `docs/architecture.md` "The head and the body"; ADR 0055 |
| the lease is 30 s | `crates/adam-runtime/src/runtime.rs:1270`, `crates/adam-service/src/service.rs:55` |
| the worker pod gets 120 s to finish | `deploy/coder/values.yaml:746` (`terminationGracePeriodSeconds`); the front's is `:28` (30) |
| SIGTERM stops the control plane first, then the workers | `docs/architecture.md` "Processes and roles" |
| App without run pods: the render fails | `deploy/coder/templates/_validate.tpl`; `docs/guides/deploy-the-coder.md` "Upgrading an App deployment" |
| a release from chart 0.4.x or earlier: `kubectl delete statefulset <release>-coder --cascade=orphan`, then `helm upgrade`; without `--cascade=orphan` the delete removes the pods | `deploy/coder/README.md:455-457` ("Upgrading from chart 0.4.x or earlier needs one extra step"); `podManagementPolicy` is immutable |
| finished runs older than 90 days are deleted by default | `docs/reference/environment.md:58` (`RUN_RETENTION_DAYS`) |
| no database backups | `docs/guides/deploy-the-coder.md` "Known risks" |

## Unverified

* That a rollback past a schema bump other than the contract case is unsafe: no test starts an older binary on a newer schema (PostgreSQL's
  `migrate` returns without error when the stored version is higher, `lib.rs:283`, so it will start).
* That one `helm upgrade` of `topology: split` satisfies "workers before the control plane": Kubernetes starts the two
  workloads' rollouts together.
* That `image.tag` accepts a `tag@digest` string.
