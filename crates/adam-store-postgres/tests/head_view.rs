//! The observer's read on Postgres: `Runtime::view` is one `lease_until` + one `load_head`, never
//! the whole state and never a body row; `Runtime::state` is the one read that joins the head and
//! its generation (one `body_list`).
//!
//! `CountingStore` records the store calls, so this is the statement shape of the read path, not a
//! timing.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_core::{BodyRow, BodyWrite, DynStore, NewRun, RunStatus, RunUpdate, Store};
use adam_runtime::{Agent, AgentError, Ctx, Inbound, Runtime, Transition};
use adam_store_postgres::PgStore;
use adam_store_testkit::counting::CountingStore;
use adam_store_testkit::fault::Method;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::oneshot;

#[derive(Clone, Serialize, Deserialize)]
struct Transcript {
    messages: Vec<String>,
    turns: u32,
}

struct SplitAgent {
    name: String,
}

#[async_trait]
impl Agent for SplitAgent {
    type State = Transcript;

    fn name(&self) -> &str {
        &self.name
    }

    fn init(&self, _input: Inbound) -> Result<Transcript, AgentError> {
        Ok(Transcript {
            // A transcript long enough that reading it would be visible: the view below is of a
            // 50-message run (49 here plus the step's) and must read no body row of it.
            messages: (0..49).map(|i| format!("message {i}")).collect(),
            turns: 0,
        })
    }

    fn split(&self, state: &Transcript) -> Option<(Transcript, Vec<Value>)> {
        let mut head = state.clone();
        let rows = std::mem::take(&mut head.messages)
            .into_iter()
            .map(|message| json!(message))
            .collect();
        Some((head, rows))
    }

    fn join(&self, head: Transcript, rows: Vec<Value>) -> Result<Transcript, AgentError> {
        let messages = rows
            .into_iter()
            .map(|row| serde_json::from_value(row).unwrap_or_default())
            .collect();
        Ok(Transcript { messages, ..head })
    }

    async fn step(
        &self,
        _ctx: &mut Ctx,
        mut state: Transcript,
    ) -> Result<Transition<Transcript>, AgentError> {
        state.messages.push("step".to_owned());
        state.turns += 1;
        Ok(Transition::Done {
            state,
            output: json!(null),
        })
    }
}

async fn wait_done(rt: &Runtime, run: adam_core::RunId) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let view = rt.view(run).await.expect("view").expect("the run");
        if view.status == RunStatus::Done {
            return;
        }
        assert!(Instant::now() < deadline, "the run never finished");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_view_reads_the_head_and_the_state_joins_the_body() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pg = PgStore::from_pool(support::pool(&url, 4))
        .with_table_prefix(&format!(
            "adam_view_{}_",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ))
        .expect("prefix");
    pg.migrate().await.expect("migrate");
    let inner: DynStore = Arc::new(pg);
    let counting = Arc::new(CountingStore::new(inner));
    let store: DynStore = counting.clone();
    let name = format!("view-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let rt = Runtime::builder(store)
        .agent(SplitAgent { name: name.clone() })
        .build();
    let run = rt
        .start(&name, Inbound::new("start", json!({})), None)
        .await
        .expect("start");
    let (stop, stopped) = oneshot::channel::<()>();
    let worker = {
        let rt = rt.clone();
        tokio::spawn(async move {
            rt.run_worker(async {
                let _ = stopped.await;
            })
            .await
            .expect("run_worker");
        })
    };
    wait_done(&rt, run).await;
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .expect("the worker stops")
        .expect("the worker task");

    // The head the run's last commit wrote is current: the view is one lease + one head.
    counting.reset();
    let view = rt.view(run).await.expect("view").expect("the run");
    assert_eq!(counting.calls(Method::LeaseUntil), 1);
    assert_eq!(counting.calls(Method::LoadHead), 1);
    assert_eq!(
        counting.calls(Method::LoadRun),
        0,
        "the view never reads the whole state"
    );
    assert_eq!(counting.calls(Method::BodyList), 0, "and never a body row");
    assert!(
        view.state["messages"].as_array().unwrap().is_empty(),
        "the head keeps no message: {:?}",
        view.state
    );
    assert_eq!(view.state["turns"], 1);

    // The one full-state read: the head and the generation it names, joined.
    counting.reset();
    let state = rt.state(run).await.expect("state").expect("the run");
    assert_eq!(counting.calls(Method::LoadHead), 1);
    assert_eq!(counting.calls(Method::BodyList), 1, "one body set");
    assert_eq!(counting.calls(Method::LoadRun), 0);
    assert_eq!(state["messages"].as_array().unwrap().len(), 50);
}

/// The head names a **prefix** of its append-only generation. A row past the head's `len` — a
/// loser's leftover, an orphan not yet collected, or a commit that landed between the head read and
/// the body read — is a later commit's, not this head's: `Runtime::state` serves the prefix and
/// does not call the head corrupt. A row missing *below* the prefix is still `Corrupt` (the
/// runtime's own test covers that; here the store is real).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_state_serves_the_head_prefix_and_ignores_a_row_past_it() {
    let Some(url) = support::test_url() else {
        return;
    };
    let prefix = format!(
        "adam_prefix_{}_",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let pg = PgStore::from_pool(support::pool(&url, 4))
        .with_table_prefix(&prefix)
        .expect("prefix");
    pg.migrate().await.expect("migrate");
    let name = format!(
        "prefix-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let run = pg
        .create_run(NewRun::new(&name, json!({})))
        .await
        .expect("create");
    let generation = uuid::Uuid::new_v4();
    let writer = uuid::Uuid::new_v4();
    let body = |messages: &[&str]| {
        messages
            .iter()
            .enumerate()
            .map(|(idx, message)| BodyRow {
                idx: u32::try_from(idx).expect("a short transcript"),
                message: json!(message),
            })
            .collect::<Vec<_>>()
    };
    pg.commit_run(
        run.id,
        run.version,
        RunUpdate::new(
            RunStatus::Runnable,
            json!({"messages": ["m0", "m1"], "turns": 1}),
        )
        .head(json!({"v": 5, "agent": {"messages": [], "turns": 1},
                   "body": {"generation": null, "len": 2}}))
        .body(BodyWrite {
            generation: Some(generation),
            rows: body(&["m0", "m1"]),
            writer,
        }),
    )
    .await
    .expect("the head and its two rows");

    // A row at `idx == the head's len`, as a lost attempt's or an append's would be.
    let client = pg.pool().get().await.expect("a connection");
    client
        .execute(
            &format!(
                "INSERT INTO {prefix}body (run_id, generation, idx, message, writer, written_at)
                 VALUES ($1, $2, 2, $3, $4, $5)"
            ),
            &[
                &run.id.0,
                &generation,
                &json!("later"),
                &uuid::Uuid::new_v4(),
                &adam_core::store::now().0,
            ],
        )
        .await
        .expect("the extra row");
    drop(client);
    assert_eq!(
        pg.body_list(run.id, generation).await.expect("rows").len(),
        3,
        "the store returns the whole generation; the runtime cuts the prefix"
    );

    let rt = Runtime::builder(Arc::new(pg) as DynStore)
        .agent(SplitAgent { name: name.clone() })
        .build();
    let state = rt.state(run.id).await.expect("state").expect("the run");
    assert_eq!(
        state,
        json!({"messages": ["m0", "m1"], "turns": 1}),
        "the head's prefix, not the row past it"
    );
}
