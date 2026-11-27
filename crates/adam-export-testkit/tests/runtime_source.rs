//! `RuntimeSource` over the in-memory store is the reference of the source suite. Any other
//! `Store` adapter reads through the same `RuntimeSource`, so its store conformance is what it
//! adds.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;

use adam_core::store::NewRun;
use adam_core::{DynStore, MemoryStore};
use adam_error::{Classify, ErrorClass};
use adam_export::{Cursor, RunSource, RuntimeSource, SourceQuery};
use adam_export_testkit::{SeedRun, SeedState};
use adam_runtime::Runtime;
use serde_json::{Value, json};

/// The stored state of `state`: the runtime's whole-state envelope (layout version 1, the agent's
/// state under `agent`), which `Runtime::state` answers as it is. A version the runtime does not
/// know is a state it cannot read.
fn stored(state: &SeedState) -> Value {
    match state {
        SeedState::Conversation(conversation) => json!({"v": 1, "agent": conversation}),
        SeedState::Other(value) => json!({"v": 1, "agent": value}),
        SeedState::Unreadable => json!({"v": 0}),
    }
}

async fn make_source(seeds: Vec<SeedRun>) -> Option<Box<dyn RunSource>> {
    let store: DynStore = Arc::new(MemoryStore::new());
    for seed in &seeds {
        let mut run = NewRun::new(seed.agent.as_str(), stored(&seed.state))
            .with_id(seed.id)
            .conversation(seed.conversation_id.as_str());
        run.status = seed.status;
        store.create_run(run).await.expect("a seeded run");
    }
    let runtime = Runtime::builder(store).build();
    let mut agents: Vec<String> = Vec::new();
    for seed in seeds {
        if !agents.contains(&seed.agent) {
            agents.push(seed.agent);
        }
    }
    Some(Box::new(RuntimeSource::new(runtime).only(agents)))
}

adam_export_testkit::source_conformance!(make_source);

/// Beyond the suite: a cursor `RuntimeSource` did not make is refused as the caller's mistake.
#[tokio::test]
async fn a_cursor_it_did_not_make_is_invalid() {
    let source = make_source(Vec::new()).await.expect("a source");
    let e = source
        .page(
            &SourceQuery::new("a", 10),
            Some(&Cursor::new("not a cursor")),
        )
        .await
        .expect_err("refused");
    assert_eq!(e.class(), ErrorClass::Invalid, "{e}");
}
