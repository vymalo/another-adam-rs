//! What the lock file must not go back to.
//!
//! The operator makes one `kube::Client` (`bin/adam-operator/src/run.rs`) and clones it into the runtime provider
//! and this crate, so one `hyper-util` connection pool serves the long watches (the `Controller`'s on
//! `AgentService` and `AgentConfig`, the provider's on workloads and pods) and the short reconcile calls beside
//! them. **hyper 1.11.1 has a race** (`client::dispatch`, fixed in 1.12.0 with `want` 0.3.2): the sender takes the
//! receiver's "I want a request" signal and then queues its request, and a receiver that polls the still empty
//! queue in between signals "I want" again. The signal stays after the request is taken, so the connection
//! reports itself ready while it is serving that request. `hyper-util`'s legacy pool then takes back an HTTP/1.1
//! connection whose response is still streaming, and the next call that checks it out is queued behind the
//! stream, with no error, for as long as the stream lives.
//!
//! *Verified 2026-10-16* in another-agentic-platform (PR #28; its `crates/controller/tests/dependencies.rs` is
//! this file's model): a kube-rs operator against a bare kube-apiserver v1.35.8 stalled 5 times in 4000 services
//! with hyper 1.11.1, never in 8000 with hyper 1.12.0 and `want` 0.3.2. *Unverified here*: no stall has been seen
//! in adam-rs, and the symptom (a reconcile call with no answer while a watch of the same pool is open) is read
//! from the code, not observed.
//!
//! Also exposed, read the same way: `adam-model-openai`, whose `stream: true` call holds an SSE response open on
//! the pool that its clones share, beside the `complete` calls and the streams of other runs (a plain `http://`
//! endpoint, or TLS that does not negotiate h2; the symptom is a transient `no response within` its timeout).
//! Not exposed: remote subagents (`adam-assembly`: a client per tool, `return_immediately` and `GetTask`, whole
//! responses), the MCP clients (`pool_max_idle_per_host(0)`), the push sender and the GitHub clients (whole short
//! responses), and `adam-env-kubernetes` (it polls, it does not watch). This test is the lock file's half of the
//! fix, in the crate whose client keeps watches open beside the calls of its reconcilers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

/// The versions of `name` in the workspace's `Cargo.lock`, as numbers (a pre-release or build suffix dropped).
fn locked(name: &str) -> Vec<Vec<u64>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut found = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line == format!("name = \"{name}\"") {
            let version = lines
                .next()
                .unwrap()
                .trim_start_matches("version = \"")
                .trim_end_matches('"');
            let numbers = version.split(['-', '+']).next().unwrap();
            found.push(numbers.split('.').map(|n| n.parse().unwrap()).collect());
        }
    }
    assert!(!found.is_empty(), "{name} is not in {}", path.display());
    found
}

/// Whether `version` is of the line that `floor` starts (the same major, or the same minor for a `0.x`): a lock may
/// hold an older line of the same crate for another dependent, which this fix does not reach.
fn same_line(version: &[u64], floor: &[u64; 3]) -> bool {
    version[0] == floor[0] && (floor[0] != 0 || version[1] == floor[1])
}

#[test]
fn hyper_is_a_version_whose_client_does_not_report_a_busy_connection_as_ready() {
    for (name, floor) in [("hyper", [1, 12, 0]), ("want", [0, 3, 2])] {
        let line: Vec<Vec<u64>> = locked(name)
            .into_iter()
            .filter(|version| same_line(version, &floor))
            .collect();
        assert!(
            !line.is_empty(),
            "{name} is locked, but in no version of the line {floor:?} belongs to: expected one, see the doc of this file"
        );
        for found in line {
            assert!(
                found.as_slice() >= floor.as_slice(),
                "{name} {found:?} is locked, and {floor:?} is the first release that withdraws a stale `want`: with \
                 anything older the connection of a watch can go back to the pool while the watch is still open, and \
                 the next call on that pool can wait behind it (see the doc of this file). \
                 `cargo update -p {name} --precise {}`",
                floor.map(|n| n.to_string()).join(".")
            );
        }
    }
}
