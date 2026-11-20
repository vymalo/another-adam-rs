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

use adam_core::{DynStore, RunStatus, Store};
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
            messages: vec!["start".to_owned()],
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
    assert_eq!(state["messages"].as_array().unwrap().len(), 2);
}
