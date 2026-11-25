# 0072. An agent's SQLite store is replicated per commit, and a new machine resumes a run from the copies

Status: **proposed (2026-11-25)**, task ANR-272, epic ANR-13 (ROADMAP M11, fourth gap: "An embedded SQLite per agent,
replicated on every commit"). **Nothing is built.** Every name and shape below is the design's, for the owner to confirm
by a dated note. It builds on [ADR 0053](0053-sqlite-a-store-service-a-database-per-agent-and-a-static-coder.md) and changes none of
its decisions.

## Context

ADR 0053's SQLite store is built (`adam-store-sqlite`, `adam#245`, amended 2026-11-20 for slice 3) and it is
**one file, one process**: `open` takes an exclusive `flock` on `<path>.lock` and refuses a second process, every change
is one `BEGIN IMMEDIATE` transaction, the journal is WAL, and `migrate` clears a dead predecessor's leases so a
`SIGKILL`ed process's runs are claimable at once. That last property is what makes "a run survives its process" true
**on one machine**.

**ADR 0053 does not decide replication.** Read at `main` on 2026-11-25, the ADR mentions neither replicating the file nor
a second machine for it; the closest sentences are the one-file/one-process rule above and the store service it defers
(decisions 4–7, not built). So the gap ANR-13 names is real and unclaimed: when the machine holding the file is lost, its
runs are lost with it.

**What must not change.** ADR 0053's one process per file stands: the replica is never a second writer, and two writers are
a failure to detect, not a configuration.

## Decision

1. **What is replicated: the store's own commits, as changesets — never the WAL file.** A copy of a live WAL is torn and
   the WAL is process-coordinated, so the unit is one transaction's changeset, captured in the store, keyed by a
   **`replica_seq`** the store increments inside the same `BEGIN IMMEDIATE` that writes the change. SQLite's session
   extension produces such a changeset, and `rusqlite` exposes it behind its **`session`** feature.
2. **Where: object storage is the durable floor; a second node is a latency copy.** One immutable object per commit, keyed
   `agents/<agent>/<replica_seq>.changeset`, plus the seq in the store's own table. A warm replica elsewhere may apply the
   same objects, but it is read-only and never opened as a store while the writer lives.
3. **When a commit is durable: the write does not return until the object store acknowledges it.** For the writes a run's
   correctness depends on (the journal put that ends a step, the lease, the thread head) the store ships, waits for the
   acknowledgement, and only then returns — one round trip per step, and that is the price of "no lost or doubled step".
   Rows that are telemetry only may ship behind the commit; the ADR says which table is in which class, and a test holds
   the classification rather than a comment.
4. **Restore and resume: a new machine downloads the objects in order, applies them to a fresh file, migrates, claims.**
   `migrate` is what ADR 0053 built: it clears the dead process's leases, so the runs become claimable with no manual step.
   A run whose last step was **not** acknowledged resumes from the last acknowledged commit, and that step runs again — the
   step's own journal record is what decides whether a re-run is safe, which is why (3) is ack-before-return for the writes
   that end a step.
5. **The failure modes, each with its rule.**
   - **A lost or corrupt object:** every object carries a checksum; a download that fails it is a failed restore, never a
     partial one. Re-uploading the same `replica_seq` is safe because the seq is in the key.
   - **The object store is unreachable:** the write fails closed — the step fails and the caller sees it, as with a
     database outage today. There is no mode in which the local file advances while the durable floor does not.
   - **Two writers:** the store keeps the highest `replica_seq` per agent in the object store's own manifest; a process
     that opens a file whose local seq is behind refuses to start (the fencing rule), in addition to ADR 0053's `flock`.
     This is what a restored copy and a still-running original look like, and the answer is "the older one stops".
   - **A replica node:** read-only, applies with the same code path as a restore, and is never a `Store` for the
     service — so it cannot become a second writer by accident.
6. **What this ADR does not cover:** the orchestrator's own SQLite store (ANR-71, the library epic), Postgres and MongoDB
   (they have their own replication), and the store service of ADR 0053 decisions 4–7.

## Outside tools, with their licences verified at source

- **SQLite** — public domain, verified 2026-11-25 at <https://www.sqlite.org/copyright.html> ("SQLite Is Public Domain",
  page last updated 2026-01-12).
- **`rusqlite`** — MIT, verified 2026-11-25 in its `Cargo.toml` (`license = "MIT"`, <https://github.com/rusqlite/rusqlite>);
  its **`session`** feature is the changeset API this design uses, and the repository already pins the crate (`0.41.1`,
  `bundled`, ADR 0053 slice 3).
- **No object-store client crate is named here.** The choice belongs to the slice that writes it, and this ADR makes no
  licence claim it has not read.

## Slices

One row per pull request, one repository per row (all in `another-adam-rs`), each with the test that proves it.

| # | Pull request | Decisive test |
|---|---|---|
| R1 | `adam-store-sqlite`: `replica_seq` and the changeset capture behind a `replicate` feature, off by default | a commit yields exactly one object whose changeset, applied to an empty file, reproduces the rows the commit wrote |
| R2 | `adam-store-sqlite`: the shipper and the ack-before-return rule, with the telemetry class named | a step whose ship is refused fails the write and the caller sees it; a step whose ship is acknowledged returns |
| R3 | `adam-store-sqlite`: restore (download in order, apply, `migrate`, claim) | **a killed node's run resumes on another from the copy with no lost or doubled step** — the gap's own test |
| R4 | `adam-store-sqlite`: the fencing rule against a second writer older than the manifest | a file whose seq is behind the object store's manifest refuses to start, naming both seqs |
| R5 | docs: `docs/architecture.md`, this repository's README and the parity row | `docs-check`, and the README states what a deployment must provide (a bucket, a credential, a name) |

R3 is the one that closes the gap; R1–R2 are its prerequisites and R4 exists so a restore can never become a second
writer.

## Consequences

- **The cost is one round trip per step**, and it is visible in the step's own timing. A deployment that cannot pay it
  sets replication off, which is today's behaviour.
- **A run that resumes may re-run the last step**, by design, and the journal record of that step is what makes it safe.
  A step whose tool is not idempotent needs its outcome written before it runs — a rule for the tool registries, not for
  this ADR.
- **The store gains a second dependency on the outside world** (a bucket), and that is why the feature is off by default.
- Nothing in ADR 0053's one-file/one-process or in the conformance suites changes.
