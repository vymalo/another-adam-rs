# adam-memory-redis

The L2 [`Memory`](../adam-memory/README.md) port on Redis: the lessons that held across L1 scopes,
shared across sessions. Redis here is **L2, a store and not an L1 cache** ([ADR 0081] decision 2):
a database reached over the network is L2 even when one agent alone uses it, so this adapter
implements `Memory` and never `SiMemory`. Within the adapter's configured `MemoryScope`, a lesson is
keyed by its audience and its root cause, and two scopes on one Redis never see each other's
lessons (decision 4).

[ADR 0081]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0081-an-agent-keeps-a-self-improvement-memory-near-it-and-what-holds-becomes-the-deployments-memory.md

The rules that decide what a lesson may hold — the scope validation, the scrub, the redaction
marker, the address check, the bounds — are `adam-memory`'s own, through
`prepare`, so this adapter and the in-process reference enforce the same rules and
the same suite proves both. Every call validates the configured scope and the audience first,
before a key is built or a connection opened: an empty one is refused, not read. A retired lesson
is kept and no longer served (decision 10): nothing is deleted.

## Keys

One Redis hash per `(scope, audience)`:

```text
<prefix>:<scope_len>:<scope>:<audience>     (prefix "adam-memory:v1" by default)
```

The field is the lesson's stored (scrubbed) root cause, the value the lesson as JSON. The length
prefix makes the scope unambiguous whatever bytes it or the audience hold, so two adapters with
two scopes over one Redis never share a hash, and the audience is the rest of the key. Override the
prefix with `with_key_prefix` when one Redis is shared with other applications.

## Concurrency

A write reads the audience's lessons, checks the bound that counts what is served, and writes one
field, under WATCH/MULTI/EXEC: `redis::transaction` watches the key, retries when it changed under
the write, and the loop gives up after 128 attempts. Retirement is the same transaction, so a
retried step lands once.

The port is `Send + Sync`, and `redis::Connection` is not `Sync`, so the adapter opens **one
connection per call**. That is a round trip's worth of cost per call and no more; a pooled or
multiplexed connection is a follow-up, and does not change what the suite proves.

## Usage

```rust
use adam_memory::{LessonSettings, MemoryScope, Scrubber};
use adam_memory_redis::RedisMemory;

let settings = LessonSettings::new(Scrubber::new(|text| text.to_owned()));
let memory = RedisMemory::open(
    "redis://127.0.0.1:6379",
    settings,
    MemoryScope::new("release-1"),
) // a URL that does not parse is refused here, not at the first lesson
.expect("the URL parses");
```

Every `RedisError` becomes a `MemoryError::Backend` (class `Transient`: an unreachable memory is a
transient tool error and the run goes on without it, ADR 0081 decision 12), except a `WRONGTYPE`
reply on a key the adapter owns, which is `MemoryError::Corrupt`: a value this adapter does not own
the shape of.

## Durability

Redis is a cache until its deployment says otherwise. A lesson store needs two settings the
adapter **documents and does not enforce** — it cannot reach the server's configuration:

* `appendonly yes` with `appendfsync everysec` (or `always` for less window at the cost of
  latency). Without AOF, a restart can lose the lessons since the last snapshot.
* `maxmemory-policy noeviction`. An eviction policy would **silently drop lessons** instead of
  refusing a write, which breaks decision 10's "kept, not deleted" without any error: bound
  checks refuse a full audience, so the server should refuse a full memory the same way.

The local stack's `redis` service ships exactly these settings (`compose.yaml`). `rediss://` works
through the bundled webpki roots (`tls-rustls-webpki-roots`).

## Tests

```sh
ADAM_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test -p adam-memory-redis
```

The suite is [`memory_conformance!`](../adam-memory-testkit/README.md), against a real Redis: two
configured scopes over one server for the decisive two-scope case. It is skipped when
`ADAM_TEST_REDIS_URL` is unset, unless `ADAM_TEST_REQUIRE_DB=1` (then it fails); CI sets the flag.
Every test process writes under its own key prefix, so a run never reads a previous run's lessons.
`tests/redis.rs` adds the adapter's own three cases: a stored value that is not a lesson is
`Corrupt`, a dead port is a `Transient` backend failure, and an empty configured scope is refused
by `recall` and `retire` before any connection (that case needs no Redis and runs with no URL
configured).

## A new required method

`Memory` is a port: a new required method breaks every implementer. Say so in the pull request and
here (CLAUDE.md rule 2). The suite in `adam-memory-testkit` is what proves an implementation.
