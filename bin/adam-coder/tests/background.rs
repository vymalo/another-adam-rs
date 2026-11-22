//! Commands that keep running (ADR 0044): `start_command`, `stop_command`, `list_commands`, read
//! with `read_output`, and gone with their run.
//!
//! The commands are real processes (`sh` in this container); the tests start them with short
//! lifetimes and sweeps through the settings, and every test stops what it starts (`sleep 6NN` is
//! only ever waiting to be stopped; each test has its own number). A test that needs a process of
//! somebody else starts its own and kills it at the end. A stop test uses a command with a child
//! and a grandchild and asserts by pid that each is gone.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_coder::tools::background::{CommandState, ListCommands, StartCommand, StopCommand};
use adam_coder::tools::named::{named_in, without_untrusted};
use adam_coder::tools::prepare::PrepareWorkspace;
use adam_coder::{AGENT_NAME, CoderAgent, CoderSettings, Redactor, ToolEnv};
use adam_core::{DynStore, MemoryStore, NewRun, RunId, RunStatus};
use adam_llm_agent::{KeptStore, ReadOutput, Tool, ToolCtx, ToolOutput};
use adam_model::{DynModel, MockModel, ModelError, ModelRequest, Park};
use adam_runtime::{CollectingSink, Inbound, Runtime};
use common::{FakeEnvironment, Fixture, MODEL_KEY, call};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// A command with a child and a grandchild in its process group: a stop that kills the leader only
/// leaves two behind.
const TREE: &str = "sh -c 'sleep 621 & wait' & echo up; wait";

struct Rig {
    fx: Fixture,
    ctx: ToolCtx,
    /// The context of each call, made once: the call of a step that runs again is the same call (the
    /// same run id in its key, so the same output id).
    calls: std::sync::Mutex<std::collections::HashMap<String, ToolCtx>>,
}

impl Rig {
    async fn with(tweak: impl FnOnce(&mut CoderSettings)) -> Self {
        Self::from(Fixture::with(tweak).await).await
    }

    async fn new() -> Self {
        Self::with(|_| {}).await
    }

    async fn from(fx: Fixture) -> Self {
        let ctx = Self::ctx_of(&fx, "start-1");
        let rig = Self {
            fx,
            ctx,
            calls: std::sync::Mutex::default(),
        };
        rig.prepare().await;
        rig
    }

    fn ctx_of(fx: &Fixture, call: &str) -> ToolCtx {
        ToolCtx::detached("tool", call, Arc::new(CollectingSink::new()))
            .with_state(fx.env.clone())
            .with_state(Arc::new(KeptStore(fx.env.kept.clone())))
    }

    /// The person names the repository and the workspace is made.
    async fn prepare(&self) {
        let run = self.ctx.run_id().to_string();
        let mut notes = self.fx.env.notes.load(&run).await.unwrap();
        notes.name_repos(named_in(
            &without_untrusted(&self.fx.remote_url()),
            &self.fx.env.settings.default_repo_host,
        ));
        self.fx.env.notes.save(&run, &notes).await.unwrap();
        let out = PrepareWorkspace
            .call(
                &self.ctx,
                json!({"repo_url": self.fx.remote_url(), "base_branch": "main"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
    }

    /// Another call of the same run.
    fn call_ctx(&self, call: &str) -> ToolCtx {
        self.calls
            .lock()
            .unwrap()
            .entry(call.to_owned())
            .or_insert_with(|| Self::ctx_of(&self.fx, call).with_root_run(self.ctx.run_id()))
            .clone()
    }

    fn run(&self) -> String {
        self.ctx.run_id().to_string()
    }

    async fn start(&self, call: &str, args: Value) -> ToolOutput {
        StartCommand.call(&self.call_ctx(call), args).await.unwrap()
    }

    async fn stop(&self, id: &str) -> ToolOutput {
        StopCommand
            .call(&self.call_ctx("stop"), json!({ "id": id }))
            .await
            .unwrap()
    }

    async fn list(&self) -> String {
        ListCommands
            .call(&self.call_ctx("list"), json!({}))
            .await
            .unwrap()
            .content
    }

    async fn read(&self, id: &str, offset: i64) -> String {
        ReadOutput
            .call(
                &self.call_ctx("read"),
                json!({"id": id, "offset": offset, "limit": 2000}),
            )
            .await
            .unwrap()
            .content
    }

    /// The id of the command a call started.
    fn id_of(&self, call: &str) -> String {
        adam_llm_agent::output_id_of(&self.call_ctx(call))
    }

    /// The pid the record of the command names.
    fn pid_of(&self, id: &str) -> u32 {
        let bytes = self
            .fx
            .env
            .kept
            .get_record(&self.run(), id)
            .unwrap()
            .expect("a record");
        let record: Value = serde_json::from_slice(&bytes).unwrap();
        u32::try_from(record["pid"].as_u64().expect("a pid")).unwrap()
    }

    fn file_of(&self, id: &str) -> std::path::PathBuf {
        self.fx
            .root
            .join("outputs")
            .join(self.run())
            .join(format!("{id}.txt"))
    }

    fn states(&self) -> Vec<(String, CommandState)> {
        self.fx.env.commands.states(&self.run())
    }

    fn worktree(&self) -> std::path::PathBuf {
        common::slot_dir(&self.fx.root, &self.run())
    }
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "))
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(20), "{what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Whether anything under `dir` holds `needle`.
fn volume_holds(dir: &Path, needle: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            volume_holds(&path, needle)
        } else {
            std::fs::read(&path).is_ok_and(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
        }
    })
}

fn last_line(text: &str) -> &str {
    text.rsplit_once('\n').map_or(text, |(_, last)| last)
}

/// The pids whose process group is `group`.
fn group_members(group: u32) -> Vec<u32> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(") ") else {
            continue;
        };
        if rest
            .split_whitespace()
            .nth(2)
            .and_then(|g| g.parse::<u32>().ok())
            == Some(group)
        {
            found.push(pid);
        }
    }
    found
}

/// The start time of a process, as the record names it.
fn start_of(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let rest = stat.rsplit_once(") ").unwrap().1;
    rest.split_whitespace().nth(19).unwrap().parse().unwrap()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

async fn until_gone(what: &str, pids: &[u32]) {
    until(what, || pids.iter().all(|p| !alive(*p))).await;
}

/// Change the record of command `id` the way another worker or the passing of time would.
fn set_record(rig: &Rig, id: &str, change: impl FnOnce(&mut Value)) {
    let kept = &rig.fx.env.kept;
    let bytes = kept.get_record(&rig.run(), id).unwrap().expect("a record");
    let mut record: Value = serde_json::from_slice(&bytes).unwrap();
    change(&mut record);
    kept.put_record(&rig.run(), id, &serde_json::to_vec(&record).unwrap())
        .unwrap();
}

fn record_of(rig: &Rig, id: &str) -> Value {
    let bytes = rig
        .fx
        .env
        .kept
        .get_record(&rig.run(), id)
        .unwrap()
        .expect("a record");
    serde_json::from_slice(&bytes).unwrap()
}

/// The owner of the record has not been heard of for ten minutes.
fn age(rig: &Rig, id: &str) {
    set_record(rig, id, |r| r["beat"] = json!(now_secs() - 600));
}

/// A run of the rig that is open in a store, so that a sweep does not take it for a finished one.
async fn open_store(rig: &Rig) -> DynStore {
    let store: DynStore = Arc::new(MemoryStore::new());
    store
        .create_run(NewRun::new(AGENT_NAME, json!({})).with_id(rig.ctx.run_id()))
        .await
        .unwrap();
    store
}

/// A worker's sweep, running until `finish`.
struct Sweeping {
    token: CancellationToken,
    handle: tokio::task::JoinHandle<Result<(), adam_error::BoxError>>,
}

impl Sweeping {
    fn of(env: &ToolEnv, store: &DynStore, every: Duration) -> Self {
        let token = CancellationToken::new();
        let handle = tokio::spawn(env.commands.clone().sweep_loop(
            store.clone(),
            token.clone(),
            every,
        ));
        Self { token, handle }
    }

    async fn finish(self) {
        self.token.cancel();
        self.handle.await.unwrap().unwrap();
    }
}

/// A second worker over the same volume and run: its own registry, its own owner.
struct Peer {
    env: Arc<ToolEnv>,
}

impl Peer {
    fn new(rig: &Rig) -> Self {
        Self {
            env: another_worker(rig),
        }
    }

    fn ctx(&self, rig: &Rig, call: &str) -> ToolCtx {
        rig.call_ctx(call)
            .with_state(self.env.clone())
            .with_state(Arc::new(KeptStore(self.env.kept.clone())))
    }

    async fn start(&self, rig: &Rig, call: &str, args: Value) -> ToolOutput {
        StartCommand.call(&self.ctx(rig, call), args).await.unwrap()
    }

    async fn stop(&self, rig: &Rig, id: &str) -> ToolOutput {
        StopCommand
            .call(&self.ctx(rig, "peer-stop"), json!({ "id": id }))
            .await
            .unwrap()
    }

    async fn list(&self, rig: &Rig) -> String {
        ListCommands
            .call(&self.ctx(rig, "peer-list"), json!({}))
            .await
            .unwrap()
            .content
    }
}

#[tokio::test]
async fn start_read_stop_locally() {
    let rig = Rig::new().await;
    let started = rig
        .start(
            "start-1",
            json!({"command": "seq 1 3000; sleep 600", "wait_for": "3000", "wait": 30}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    assert!(
        started.content.contains(&format!(
            "[running as {id}; read_output {{\"id\":\"{id}\"}} reads its output, stop_command {{\"id\":\"{id}\"}} stops it]"
        )),
        "{}",
        started.content
    );
    assert!(
        started.content.contains("running for"),
        "{}",
        started.content
    );
    let pid = rig.pid_of(&id);
    assert!(alive(pid));

    // The first page is full, and the last page says it is not the end.
    let first = rig.read(&id, 1).await;
    assert!(first.starts_with("1\n2\n3\n"), "{}", &first[..20]);
    assert_eq!(
        last_line(&first),
        "[more: 1000 lines left; next page: offset=2001]"
    );
    let second = rig.read(&id, 2001).await;
    assert_eq!(
        last_line(&second),
        "[running: 3000 lines so far; next page: offset=3001]"
    );
    assert_eq!(second, rig.read(&id, 2001).await, "the same bytes twice");
    // Where the next page starts: nothing yet, and the same line.
    let after = rig.read(&id, 3001).await;
    assert_eq!(
        after,
        "[running: 3000 lines so far; next page: offset=3001]"
    );

    let list = rig.list().await;
    assert!(
        list.contains(&format!("{id} running")) && list.contains("3000 lines"),
        "{list}"
    );
    assert!(list.contains("seq 1 3000; sleep 600"), "{list}");

    let stopped = rig.stop(&id).await;
    assert!(!stopped.is_error, "{}", stopped.content);
    assert!(
        stopped.content.contains("stopped: stop_command"),
        "{}",
        stopped.content
    );
    assert!(stopped.content.contains("\n3000\n"), "{}", stopped.content);
    assert!(
        stopped.content.contains(&format!(
            "read_output {{\"id\":\"{id}\"}} reads them page by page]"
        )),
        "{}",
        stopped.content
    );
    until("the process to be gone", || !alive(pid)).await;
    // The file is finished: the same page now ends, and the same call is idempotent.
    assert_eq!(last_line(&rig.read(&id, 2001).await), "[end: 3000 lines]");
    let again = rig.stop(&id).await;
    assert!(
        again.content.contains("stopped: stop_command"),
        "{}",
        again.content
    );
    assert!(rig.list().await.contains("stopped: stop_command"));
}

#[tokio::test]
async fn a_command_that_ends_inside_the_wait_gives_the_result_of_run() {
    let rig = Rig::new().await;
    let out = rig
        .start("start-1", json!({"command": "echo hi; exit 3"}))
        .await;
    assert!(
        out.content
            .starts_with("$ echo hi; exit 3\nexit code 3\n--- output ---\nhi\n"),
        "{}",
        out.content
    );
    let id = rig.id_of("start-1");
    assert!(rig.list().await.contains(&format!("{id} exited 3 after")));
    assert!(rig.states().iter().all(|(_, s)| !s.is_running()));
}

#[tokio::test]
async fn a_start_run_again_with_the_same_call_starts_one_process() {
    let rig = Rig::new().await;
    let work = rig.worktree();
    let command = "echo started >> started.txt; sleep 600";
    let first = rig
        .start("start-1", json!({"command": command, "wait": 1}))
        .await;
    assert!(!first.is_error, "{}", first.content);
    let id = rig.id_of("start-1");
    let pid = rig.pid_of(&id);
    until("the line to be written", || {
        work.join("started.txt").exists()
    })
    .await;

    // The step runs again in this process (its result was never journaled): the same command.
    let again = rig
        .start("start-1", json!({"command": command, "wait": 1}))
        .await;
    assert!(!again.is_error, "{}", again.content);
    assert_eq!(rig.pid_of(&id), pid, "the same process");
    assert_eq!(
        std::fs::read_to_string(work.join("started.txt")).unwrap(),
        "started\n",
        "nothing was started twice"
    );
    assert_eq!(rig.states().len(), 1);
    rig.stop(&id).await;
    until("it to be gone", || !alive(pid)).await;
}

/// A second worker over the same volume, as a restarted process is one.
fn another_worker(rig: &Rig) -> Arc<ToolEnv> {
    another_env(&rig.fx)
}

fn another_env(fx: &Fixture) -> Arc<ToolEnv> {
    let first = &fx.env;
    Arc::new(
        ToolEnv::new(
            first.workspaces.clone(),
            first.code_host.clone(),
            first.settings.clone(),
        )
        .with_redactor(Redactor::new(common::SECRETS)),
    )
}

#[tokio::test]
async fn a_replay_in_another_process_kills_the_old_one_and_starts_again() {
    let rig = Rig::new().await;
    let work = rig.worktree();
    let command = "echo started >> started.txt; sleep 600";
    rig.start("start-1", json!({"command": command, "wait": 1}))
        .await;
    let id = rig.id_of("start-1");
    let old = rig.pid_of(&id);
    until("the line", || work.join("started.txt").exists()).await;
    // The worker that started it is gone: it has not beat for ten minutes.
    age(&rig, &id);

    // The step runs again on another worker: another owner, the same call.
    let env = another_worker(&rig);
    let ctx = rig
        .call_ctx("start-1")
        .with_state(env.clone())
        .with_state(Arc::new(KeptStore(env.kept.clone())));
    let out = StartCommand
        .call(&ctx, json!({"command": command, "wait": 1}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    until("the earlier process to be gone", || !alive(old)).await;
    let new = {
        let bytes = env.kept.get_record(&rig.run(), &id).unwrap().unwrap();
        let record: Value = serde_json::from_slice(&bytes).unwrap();
        u32::try_from(record["pid"].as_u64().unwrap()).unwrap()
    };
    assert_ne!(new, old);
    assert!(alive(new));
    // The earlier process wrote its line, the new one its own; the output was begun again.
    assert_eq!(
        std::fs::read_to_string(work.join("started.txt"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    let owner: Value = {
        let bytes = env.kept.get_record(&rig.run(), &id).unwrap().unwrap();
        serde_json::from_slice(&bytes).unwrap()
    };
    assert_eq!(owner["owner"], env.commands.owner());
    let stop = StopCommand.call(&ctx, json!({ "id": id })).await.unwrap();
    assert!(!stop.is_error, "{}", stop.content);
    until("the new process to be gone", || !alive(new)).await;
}

#[tokio::test]
async fn a_restarted_worker_says_gone_and_kills_by_exec_id() {
    let fake = FakeEnvironment::new(None);
    let rig = Rig::from(Fixture::new().await.using(fake.clone())).await;
    let started = rig
        .start("start-1", json!({"command": "sleep 600", "wait": 1}))
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    let pid = rig.pid_of(&id);
    let exec = fake.session.prepared.lock().unwrap()[0].clone();
    assert!(exec.is_background(), "{exec}");
    // The worker is gone: it has not beat for ten minutes.
    age(&rig, &id);

    // A new process serves the run: it holds none of the first one's commands.
    let env = another_worker(&rig);
    let env = {
        let mut env = Arc::try_unwrap(env).ok().expect("not shared");
        env.environment = fake.clone();
        Arc::new(env)
    };
    let ctx = ToolCtx::detached("tool", "list", Arc::new(CollectingSink::new()))
        .with_state(env.clone())
        .with_root_run(rig.ctx.run_id());
    let listed = ListCommands.call(&ctx, json!({})).await.unwrap().content;
    assert!(
        listed.contains(&format!(
            "{id} gone: the worker that ran it is gone; start it again"
        )),
        "{listed}"
    );
    assert!(
        fake.session.killed.lock().unwrap().contains(&exec),
        "its exec id was killed in the environment"
    );
    until("the process this machine held to be gone", || !alive(pid)).await;
    // What it had printed so far stays readable.
    let ctx = ctx.with_state(Arc::new(KeptStore(env.kept.clone())));
    let read = ReadOutput
        .call(&ctx, json!({"id": id}))
        .await
        .unwrap()
        .content;
    assert_eq!(read, "[end: 0 lines]");
    // Stopping it says how it ended, from the record.
    let stopped = StopCommand.call(&ctx, json!({ "id": id })).await.unwrap();
    assert!(stopped.content.contains("gone:"), "{}", stopped.content);
}

#[tokio::test]
async fn a_pod_command_is_stopped_through_adam_exec_kill() {
    let fake = FakeEnvironment::new(None);
    fake.session.as_pod(Some(250), Some(512 * 1024 * 1024));
    let rig = Rig::from(Fixture::new().await.using(fake.clone())).await;
    let started = rig
        .start("start-1", json!({"command": "sleep 600", "wait": 1}))
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    let exec = fake.session.prepared.lock().unwrap()[0].clone();
    assert!(exec.as_str().starts_with("bg-"), "{exec}");
    assert!(
        fake.session.specs.lock().unwrap()[0].background,
        "the spec says it outlives its call"
    );
    let pid = rig.pid_of(&id);
    let stopped = rig.stop(&id).await;
    assert!(!stopped.is_error, "{}", stopped.content);
    assert!(
        fake.session.killed.lock().unwrap().contains(&exec),
        "{:?}",
        fake.session.killed.lock().unwrap()
    );
    until("the client to be gone", || !alive(pid)).await;
}

#[tokio::test]
async fn output_is_scrubbed_before_it_is_written() {
    // A secret of several lines, as a private key is, split across two reads: nothing of it is on
    // the volume at any time.
    let secret = "-----BEGIN KEY-----\nAAAA1234BBBB5678\n-----END KEY-----";
    let mut fx = Fixture::new().await;
    let first = fx.env.clone();
    fx.env = Arc::new(
        ToolEnv::new(
            first.workspaces.clone(),
            first.code_host.clone(),
            first.settings.clone(),
        )
        .with_redactor(Redactor::new([secret, MODEL_KEY])),
    );
    let rig = Rig::from(fx).await;
    // The command never holds the secret whole, as the output will.
    let command = "printf 'before\\n%s%s\\n' '-----BEG' 'IN KEY-----'; sleep 1; printf '%s%s\\n%s%s\\nready\\n' 'AAAA1234' 'BBBB5678' '-----EN' 'D KEY-----'; echo key=sk-live-0123456789abcdefSECRETKEY; sleep 600";
    let started = rig
        .start("start-1", json!({"command": command, "wait": 1}))
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    // The first read is in, the second is not: what could be the start of the secret is held back.
    assert!(started.content.contains("before"), "{}", started.content);
    assert!(
        !started.content.contains("BEGIN KEY"),
        "{}",
        started.content
    );
    assert!(
        !volume_holds(&rig.fx.root.join("outputs"), "BEGIN KEY"),
        "the start of the secret was written"
    );
    let done = rig
        .start(
            "start-1",
            json!({"command": command, "wait": 20, "wait_for": "ready"}),
        )
        .await;
    assert!(done.content.contains("before"), "{}", done.content);
    until("the second line to be written", || {
        std::fs::read_to_string(rig.file_of(&id)).is_ok_and(|t| t.contains("key="))
    })
    .await;
    let on_disk = std::fs::read_to_string(rig.file_of(&id)).unwrap();
    assert!(on_disk.contains("[redacted]"), "{on_disk}");
    assert!(on_disk.contains("key=[redacted]"), "{on_disk}");
    for needle in ["BEGIN KEY", "AAAA1234BBBB5678", "END KEY", MODEL_KEY] {
        assert!(
            !volume_holds(&rig.fx.root, needle),
            "{needle} is on the volume"
        );
    }
    let page = rig.read(&id, 1).await;
    assert!(
        !page.contains("BEGIN KEY") && !page.contains(MODEL_KEY),
        "{page}"
    );
    rig.stop(&id).await;
    for needle in ["BEGIN KEY", "AAAA1234BBBB5678", "END KEY", MODEL_KEY] {
        assert!(
            !volume_holds(&rig.fx.root, needle),
            "{needle} is on the volume"
        );
    }
}

#[tokio::test]
async fn output_stops_at_the_cap() {
    let rig = Rig::with(|s| s.background_output_max = 10 * 1024).await;
    let started = rig
        .start(
            "start-1",
            json!({"command": "seq 1 100000; sleep 600", "wait_for": "100000", "wait": 30}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    let stopped = rig.stop(&id).await;
    // The end of the output is kept in memory, whatever the file keeps.
    assert!(
        stopped.content.contains("\n100000\n"),
        "{}",
        stopped.content
    );
    assert!(
        stopped.content.contains("(the output was cut)"),
        "{}",
        stopped.content
    );
    let size = std::fs::metadata(rig.file_of(&id)).unwrap().len();
    assert!(size > 9 * 1024 && size < 11 * 1024, "{size}");
    let page = rig.read(&id, -3).await;
    assert!(page.contains("(output cut: only the first"), "{page}");
    assert!(page.contains("of 588895 bytes are kept"), "{page}");
    assert!(page.ends_with(']'), "{page}");
}

#[tokio::test]
async fn the_lifetime_ends_it() {
    let rig = Rig::with(|s| s.background_command_max = Duration::from_secs(1)).await;
    let started = rig
        .start("start-1", json!({"command": "sleep 600", "wait": 1}))
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    let pid = rig.pid_of(&id);
    until("it to end", || {
        rig.states().iter().any(
            |(_, s)| matches!(s, CommandState::Stopped { why } if why.contains("lifetime of 1 s")),
        )
    })
    .await;
    until("the process to be gone", || !alive(pid)).await;
    assert!(
        rig.list()
            .await
            .contains("stopped: its lifetime of 1 s ended")
    );
}

#[tokio::test]
async fn more_than_the_maximum_is_refused() {
    let rig = Rig::with(|s| s.background_commands_max = 2).await;
    for call in ["a", "b"] {
        let out = rig
            .start(call, json!({"command": "sleep 600", "wait": 1}))
            .await;
        assert!(!out.is_error, "{}", out.content);
    }
    let refused = rig
        .start("c", json!({"command": "sleep 600", "wait": 1}))
        .await;
    assert!(
        refused.is_error
            && refused.content.contains("at most 2")
            && refused.content.contains("stop_command"),
        "{}",
        refused.content
    );
    assert_eq!(rig.fx.env.commands.running_count(&rig.run()), 2);
    // Stopping one makes room.
    rig.stop(&rig.id_of("a")).await;
    let third = rig
        .start("c", json!({"command": "sleep 600", "wait": 1}))
        .await;
    assert!(!third.is_error, "{}", third.content);
    for call in ["b", "c"] {
        rig.stop(&rig.id_of(call)).await;
    }
    assert_eq!(rig.fx.env.commands.running_count(&rig.run()), 0);
}

#[tokio::test]
async fn bad_arguments_and_a_stranger_are_refused() {
    let rig = Rig::new().await;
    for (args, said) in [
        (json!({"command": " "}), "command is required"),
        (json!({"command": "true", "wait": 0}), "from 1 to 120"),
        (json!({"command": "true", "wait": 121}), "from 1 to 120"),
        (
            json!({"command": "true", "wait_for": "a\nb"}),
            "wait_for is one line",
        ),
        (json!({"command": "true", "cwd": "../x"}), "cwd"),
    ] {
        let out = rig.start("bad", args.clone()).await;
        assert!(
            out.is_error && out.content.contains(said),
            "{args}: {}",
            out.content
        );
    }
    let out = rig.stop("not-an-id").await;
    assert!(
        out.is_error && out.content.contains("not a command id"),
        "{}",
        out.content
    );
    let out = rig.stop("o-0123456789abcdef").await;
    assert!(
        out.is_error && out.content.contains("no command"),
        "{}",
        out.content
    );
    assert!(rig.list().await.starts_with("No command was started"));
}

/// The configuration of the slot's repository, as `run` holds a command to leave it.
fn git_config(dir: &Path) -> String {
    common::git(dir, &["config", "--local", "--list"])
}

#[tokio::test]
async fn a_changed_configuration_is_put_back_and_a_moved_ref_is_reported() {
    let rig = Rig::new().await;
    let work = rig.worktree();
    let before = git_config(&work);
    let started = rig
        .start(
            "start-1",
            json!({"command": "git config user.nickname watcher; git config core.hooksPath /nonexistent; git config core.fsmonitor 'touch /nonexistent/x'; echo ready; sleep 628", "wait_for": "ready", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    assert!(git_config(&work).contains("core.hookspath"), "it ran");
    let stopped = rig.stop(&id).await;
    assert!(
        stopped
            .content
            .contains("the git configuration was changed while it ran and has been put back"),
        "{}",
        stopped.content
    );
    assert_eq!(git_config(&work), before, "the configuration is as it was");
    assert!(rig.list().await.contains("(git changed while it ran)"));

    // A ref that moved is reported and left alone: it may be the run's own commit.
    let second = rig
        .start(
            "start-2",
            json!({"command": "git branch made-by-the-watcher; echo ready; sleep 629", "wait_for": "ready", "wait": 20}),
        )
        .await;
    assert!(!second.is_error, "{}", second.content);
    let stopped = rig.stop(&rig.id_of("start-2")).await;
    assert!(
        stopped.content.contains("HEAD, a ref or `.git` differ"),
        "{}",
        stopped.content
    );
    assert!(
        common::git(&work, &["branch", "--list", "made-by-the-watcher"])
            .contains("made-by-the-watcher"),
        "not undone"
    );
}

/// What a run's guard of `run_command` and `run` says about commands that keep running, on every
/// refusal path.
#[tokio::test]
async fn every_refusal_of_run_command_and_run_names_the_commands_that_run_in_the_slot() {
    use adam_coder::tools::inspect::RunCommand;
    use adam_coder::tools::make::Run;

    let rig = Rig::new().await;
    let started = rig
        .start(
            "start-1",
            json!({"command": "echo ready; sleep 630", "wait_for": "ready", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    let named = |text: &str| {
        text.contains("Commands that keep running are writing in this slot") && text.contains(&id)
    };

    // A command that changes a file: `run_command` undoes it.
    let out = RunCommand
        .call(
            &rig.call_ctx("look"),
            json!({"command": "echo changed > README.md"}),
        )
        .await
        .unwrap();
    assert!(out.is_error && named(&out.content), "{}", out.content);
    // A command that changes git: `run` undoes it.
    let out = Run
        .call(
            &rig.call_ctx("make"),
            json!({"command": "git config user.nickname somebody"}),
        )
        .await
        .unwrap();
    assert!(out.is_error && named(&out.content), "{}", out.content);

    // HEAD is not on the run's own branch: nothing is run, in either tool.
    common::git(&rig.worktree(), &["checkout", "--quiet", "--detach"]);
    for (call, tool) in [("look-2", "run_command"), ("make-2", "run")] {
        let ctx = rig.call_ctx(call);
        let out = if tool == "run" {
            Run.call(&ctx, json!({"command": "true"})).await.unwrap()
        } else {
            RunCommand
                .call(&ctx, json!({"command": "true"}))
                .await
                .unwrap()
        };
        assert!(
            out.is_error && out.content.contains("Nothing was run") && named(&out.content),
            "{tool}: {}",
            out.content
        );
    }
    rig.stop(&id).await;
}

#[tokio::test]
async fn a_worktree_that_cannot_be_read_at_the_end_says_nothing_about_git() {
    let rig = Rig::new().await;
    let started = rig
        .start(
            "start-1",
            json!({"command": "echo ready; sleep 631", "wait_for": "ready", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    // The worktree goes away (the janitor, a command): git cannot be read at the end.
    std::fs::remove_dir_all(rig.worktree()).unwrap();
    let stopped = rig.stop(&id).await;
    assert!(!stopped.is_error, "{}", stopped.content);
    assert!(
        !stopped.content.contains("Note:") && !stopped.content.contains("git"),
        "{}",
        stopped.content
    );
    assert!(!rig.list().await.contains("git changed"));
}

#[tokio::test]
async fn a_command_is_never_started_in_a_reference() {
    use adam::agent_fs::ReferenceDef;
    use adam_coder::tools::files::ReadFile;
    let fx = Fixture::new().await;
    let design = fx.extra_remote("design", &[("tokens.css", "--blue: #00f;\n")]);
    let url = design.to_string_lossy().into_owned();
    let fx = fx.referencing(vec![ReferenceDef {
        name: "design".into(),
        url,
        branch: None,
        description: "Tokens".into(),
    }]);
    let rig = Rig::from(fx).await;
    // The first read makes the reference a slot of the run.
    let read = ReadFile
        .call(
            &rig.call_ctx("read-1"),
            json!({"path": "tokens.css", "repo": "design"}),
        )
        .await
        .unwrap();
    assert!(!read.is_error, "{}", read.content);
    assert!(rig.worktree().parent().unwrap().join("design").is_dir());
    let marker = rig.worktree().parent().unwrap().join("design/started.txt");
    let out = rig
        .start(
            "start-1",
            json!({"command": "echo started > started.txt; sleep 647", "repo": "design", "wait": 1}),
        )
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("is a reference"), "{}", out.content);
    assert!(!marker.exists(), "nothing was started in the reference");
    assert!(rig.states().is_empty());
}

#[tokio::test]
async fn the_tiers_say_who_may_start_and_who_may_only_look() {
    use adam_a2a::AccessTier::{Edits, Plan};
    assert_eq!(adam_coder::tier::needs("start_command"), Edits);
    assert_eq!(adam_coder::tier::needs("stop_command"), Plan);
    assert_eq!(adam_coder::tier::needs("list_commands"), Plan);
}

// ------------------------------------------------------------------ the run ends, the commands end

/// A coder on `fx` over a store the test holds, with `script` for the model, and the run's id.
struct Driven {
    rt: Runtime,
    store: DynStore,
    mock: Arc<MockModel>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<tokio::task::JoinHandle<Result<(), adam_runtime::RuntimeError>>>,
}

impl Driven {
    fn new(fx: &Fixture, script: impl FnOnce(&MockModel)) -> Self {
        Self::with_env(fx.env.clone(), script)
    }

    /// The run is stepped by the worker whose tools are `env`.
    fn with_env(env: Arc<ToolEnv>, script: impl FnOnce(&MockModel)) -> Self {
        let mock = Arc::new(MockModel::new());
        script(&mock);
        let model: DynModel = mock.clone();
        let agent = CoderAgent::new(model, "test-model", env);
        let store: DynStore = Arc::new(MemoryStore::new());
        let rt = Runtime::builder(store.clone())
            .agent(agent)
            .poll_interval(Duration::from_millis(20))
            .build();
        let (stop, rx) = tokio::sync::oneshot::channel::<()>();
        let worker = {
            let rt = rt.clone();
            tokio::spawn(async move {
                rt.run_worker(async {
                    let _ = rx.await;
                })
                .await
            })
        };
        Self {
            rt,
            store,
            mock,
            stop: Some(stop),
            worker: Some(worker),
        }
    }

    async fn start(&self, text: &str) -> RunId {
        self.rt
            .start(
                AGENT_NAME,
                Inbound::new("message", json!({ "text": text })),
                None,
            )
            .await
            .unwrap()
    }

    async fn settle(&self, run: RunId) -> RunStatus {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let view = self.rt.view(run).await.unwrap().expect("run exists");
            match view.status {
                RunStatus::Done | RunStatus::Failed => return view.status,
                RunStatus::Parked if view.waiting => return view.status,
                _ => {}
            }
            assert!(Instant::now() < deadline, "timed out: {view:#?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn finish(mut self) -> Vec<ModelRequest> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
        self.mock.requests()
    }
}

fn stopped_why(fx: &Fixture, run: RunId) -> Vec<String> {
    fx.env
        .commands
        .states(&run.to_string())
        .into_iter()
        .filter_map(|(_, state)| match state {
            CommandState::Stopped { why } => Some(why),
            _ => None,
        })
        .collect()
}

/// A refusal whose provider parks the run (ADR 0067 decision 3).
fn refused(status: u16, slug: Option<&str>, park: Park) -> ModelError {
    ModelError::Refused {
        status,
        slug: slug.map(str::to_owned),
        retry_after: None,
        park,
    }
}

#[tokio::test]
async fn a_finished_root_run_stops_its_commands() {
    let fx = Fixture::new().await;
    let url = fx.remote_url();
    let driven = Driven::new(&fx, |m| {
        m.push_tool_calls(vec![call(
            "c1",
            "prepare_workspace",
            json!({"repo_url": url, "base_branch": "main"}),
        )]);
        m.push_tool_calls(vec![call(
            "c2",
            "start_command",
            json!({"command": "sleep 600", "wait": 1}),
        )]);
        m.push_text("Looked around; nothing to change.");
    });
    let run = driven.start(&format!("look at {url}")).await;
    let status = driven.settle(run).await;
    assert_eq!(status, RunStatus::Done);
    // At once, in the agent's step: nothing waited for the sweep.
    let why = stopped_why(&fx, run);
    assert_eq!(why, ["the run ended"], "{status:?}");
    assert_eq!(fx.env.commands.running_count(&run.to_string()), 0);
    driven.finish().await;
}

#[tokio::test]
async fn a_park_on_the_person_stops_the_commands() {
    let fx = Fixture::new().await;
    let url = fx.remote_url();
    let driven = Driven::new(&fx, |m| {
        m.push_tool_calls(vec![call(
            "c1",
            "prepare_workspace",
            json!({"repo_url": url, "base_branch": "main"}),
        )]);
        m.push_tool_calls(vec![call(
            "c2",
            "start_command",
            json!({"command": "sleep 600", "wait": 1}),
        )]);
        m.push_tool_calls(vec![call(
            "c3",
            "ask_user",
            json!({"question": "Shall I go on?"}),
        )]);
    });
    let run = driven.start(&format!("look at {url}")).await;
    let status = driven.settle(run).await;
    assert_eq!(status, RunStatus::Parked);
    assert_eq!(
        stopped_why(&fx, run),
        ["the run waits for the person's answer"]
    );
    driven.finish().await;
}

#[tokio::test]
async fn a_park_until_woken_stops_the_commands() {
    let fx = Fixture::new().await;
    let url = fx.remote_url();
    let driven = Driven::new(&fx, |m| {
        m.push_tool_calls(vec![call(
            "c1",
            "prepare_workspace",
            json!({"repo_url": url, "base_branch": "main"}),
        )]);
        m.push_tool_calls(vec![call(
            "c2",
            "start_command",
            json!({"command": "sleep 660", "wait": 1}),
        )]);
        // The person's usage limit is reached: the run waits until they raise it, which may take
        // a while, so a server nobody can use is not kept (ADR 0067).
        m.push_error(refused(402, Some("over_limit"), Park::UntilWoken));
    });
    let run = driven.start(&format!("look at {url}")).await;
    let status = driven.settle(run).await;
    assert_eq!(status, RunStatus::Parked);
    assert_eq!(
        stopped_why(&fx, run),
        ["the run waits on the model until it is woken"]
    );
    assert_eq!(fx.env.commands.running_count(&run.to_string()), 0);
    driven.finish().await;
}

#[tokio::test]
async fn a_cancelled_run_is_reached_by_the_sweep() {
    let fx = Fixture::new().await;
    let url = fx.remote_url();
    let driven = Driven::new(&fx, |m| {
        m.push_tool_calls(vec![call(
            "c1",
            "prepare_workspace",
            json!({"repo_url": url, "base_branch": "main"}),
        )]);
        m.push_tool_calls(vec![call(
            "c2",
            "start_command",
            json!({"command": "sleep 600", "wait": 1}),
        )]);
        // The run is busy with a long call when it is cancelled.
        m.push_tool_calls(vec![call(
            "c3",
            "run_command",
            json!({"command": "sleep 30"}),
        )]);
    });
    let run = driven.start(&format!("look at {url}")).await;
    let run_id = run.to_string();
    until("the command to run", || {
        fx.env.commands.running_count(&run_id) == 1
    })
    .await;
    // A cancel commits `Failed` and never calls the agent's step.
    driven
        .rt
        .cancel(run, "cancelled by the test")
        .await
        .unwrap();
    assert_eq!(driven.settle(run).await, RunStatus::Failed);
    let pid = {
        let id = fx.env.commands.states(&run_id)[0].0.clone();
        let bytes = fx.env.kept.get_record(&run_id, &id).unwrap().unwrap();
        let record: Value = serde_json::from_slice(&bytes).unwrap();
        u32::try_from(record["pid"].as_u64().unwrap()).unwrap()
    };
    assert!(alive(pid), "nothing has looked at the run yet");
    // The sweep reads the run's standing and stops what it left.
    let stopped = fx.env.commands.sweep(driven.store.as_ref()).await;
    assert_eq!(stopped, 1);
    assert_eq!(stopped_why(&fx, run), ["the run ended"]);
    until("the process to be gone", || !alive(pid)).await;
    driven.finish().await;
}

#[tokio::test]
async fn three_identical_reads_of_a_quiet_command_are_refused() {
    let fx = Fixture::new().await;
    let url = fx.remote_url();
    let id = Arc::new(std::sync::Mutex::new(String::new()));
    let driven = Driven::new(&fx, |m| {
        m.push_tool_calls(vec![call(
            "c1",
            "prepare_workspace",
            json!({"repo_url": url, "base_branch": "main"}),
        )]);
        m.push_tool_calls(vec![call(
            "c2",
            "start_command",
            json!({"command": "echo up; sleep 600", "wait_for": "up", "wait": 20}),
        )]);
    });
    let run = driven.start(&format!("look at {url}")).await;
    let run_id = run.to_string();
    until("the command to run", || {
        fx.env.commands.running_count(&run_id) == 1
    })
    .await;
    *id.lock().unwrap() = adam_llm_agent::output_id(&run_id, "c2");
    // The model reads the quiet command three times with the same arguments.
    let read = |n: u32| {
        call(
            &format!("r{n}"),
            "read_output",
            json!({"id": id.lock().unwrap().clone(), "offset": 2}),
        )
    };
    driven.mock.push_tool_calls(vec![read(1)]);
    driven.mock.push_tool_calls(vec![read(2)]);
    driven.mock.push_tool_calls(vec![read(3)]);
    driven.mock.push_text("It is quiet.");
    let _ = driven.settle(run).await;
    let requests = driven.finish().await;
    let last = requests.last().unwrap();
    let results: Vec<String> = last
        .messages
        .iter()
        .filter_map(|m| match m {
            adam_model::Message::Tool {
                call_id, content, ..
            } if call_id.starts_with('r') => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 3, "{results:?}");
    assert_eq!(results[0], results[1]);
    assert!(
        results[0].starts_with("[running: 1 lines so far; next page: offset=2]"),
        "{results:?}"
    );
    assert!(results[2].contains("third call in a row"), "{results:?}");
    fx.env.commands.stop_run(&run_id, "the test is over").await;
}

// ---------------------------------------------------------------------- processes: the whole group

/// The process group is killed by a stop: the child and the grandchild of the shell go with it.
#[tokio::test]
async fn a_stop_kills_the_leader_its_child_and_its_grandchild() {
    let rig = Rig::new().await;
    let started = rig
        .start(
            "start-1",
            json!({"command": TREE, "wait_for": "up", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    let leader = rig.pid_of(&id);
    until("the tree to be there", || group_members(leader).len() >= 3).await;
    let tree = group_members(leader);
    let stopped = rig.stop(&id).await;
    assert!(!stopped.is_error, "{}", stopped.content);
    until_gone("every process of the group to be gone", &tree).await;
}

/// A leader that ends by itself leaves nothing running in its group.
#[tokio::test]
async fn a_leader_that_ends_by_itself_leaves_no_child_behind() {
    let rig = Rig::new().await;
    let out = rig
        .start(
            "start-1",
            json!({"command": "sleep 622 & echo child=$!", "wait": 20}),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    let child: u32 = out
        .content
        .lines()
        .find_map(|line| line.strip_prefix("child="))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("the child's pid: {}", out.content));
    until_gone("the child of the leader to be gone", &[child]).await;
}

/// In a run pod the client's exit does not end what `cmd &` left there: the exec id is
/// killed in the environment on exit as well.
#[tokio::test]
async fn a_command_that_ends_by_itself_is_still_killed_in_the_environment() {
    let fake = FakeEnvironment::new(None);
    fake.session.as_pod(Some(250), Some(512 * 1024 * 1024));
    let rig = Rig::from(Fixture::new().await.using(fake.clone())).await;
    let out = rig
        .start("start-1", json!({"command": "echo done", "wait": 20}))
        .await;
    assert!(out.content.contains("exit code 0"), "{}", out.content);
    let exec = fake.session.prepared.lock().unwrap()[0].clone();
    assert!(
        fake.session.killed.lock().unwrap().contains(&exec),
        "{:?}",
        fake.session.killed.lock().unwrap()
    );
}

/// An exec that does not answer does not hold the command: the stop is done after the bound.
#[tokio::test]
async fn a_kill_that_never_answers_does_not_hold_a_stop() {
    let fake = FakeEnvironment::new(None);
    let rig = Rig::from(
        Fixture::with(|s| s.background_kill_wait = Duration::from_millis(300))
            .await
            .using(fake.clone()),
    )
    .await;
    fake.session
        .kill_hangs
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let started = rig
        .start("start-1", json!({"command": "sleep 632", "wait": 1}))
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    let pid = rig.pid_of(&id);
    let began = Instant::now();
    let stopped = rig.stop(&id).await;
    assert!(
        !stopped.is_error && stopped.content.contains("stopped: stop_command"),
        "{}",
        stopped.content
    );
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "{:?}",
        began.elapsed()
    );
    until_gone("the process", &[pid]).await;
}

/// A record that names a live process of somebody else (another start time) never gets it killed.
#[tokio::test]
async fn a_pid_with_another_start_time_is_never_killed_and_one_with_its_own_is() {
    use std::os::unix::process::CommandExt as _;
    let rig = Rig::new().await;
    let peer = Peer::new(&rig);
    let spawn = |seconds: &str| {
        std::process::Command::new("sleep")
            .arg(seconds)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    };
    let mut stranger = spawn("633");
    let mut ghost = spawn("634");
    let _reap = (
        common::GroupGuard(stranger.id()),
        common::GroupGuard(ghost.id()),
    );
    let record = |id: &str, pid: u32, start: u64| {
        json!({
            "command": "ghost", "slot": "remote", "cwd": "", "exec": format!("bg-ghost-{id}"),
            "owner": "a-worker-that-is-gone", "pid": pid, "start": start, "started_at": 1,
            "state": "running", "ran_secs": null, "lines": 0, "git": null, "beat": 0,
            "host": rig.fx.env.commands.host(),
        })
    };
    let (wrong, right) = (
        adam_llm_agent::output_id(&rig.run(), "wrong"),
        adam_llm_agent::output_id(&rig.run(), "right"),
    );
    for (id, pid, start) in [
        (&wrong, stranger.id(), start_of(stranger.id()) + 1),
        (&right, ghost.id(), start_of(ghost.id())),
    ] {
        rig.fx
            .env
            .kept
            .put_record(
                &rig.run(),
                id,
                &serde_json::to_vec(&record(id, pid, start)).unwrap(),
            )
            .unwrap();
    }
    let listed = peer.list(&rig).await;
    assert!(
        listed.contains(&format!("{wrong} gone")) && listed.contains(&format!("{right} gone")),
        "{listed}"
    );
    until("the process the record names to be killed", || {
        ghost.try_wait().unwrap().is_some()
    })
    .await;
    assert!(
        stranger.try_wait().unwrap().is_none(),
        "a process with another start time is not the record's"
    );
    stranger.kill().unwrap();
    stranger.wait().unwrap();
}

/// What a leader left in its group is killed when the record is settled, whether or not the leader
/// is still there.
#[tokio::test]
async fn what_is_left_of_a_group_whose_leader_is_gone_is_killed() {
    use std::os::unix::process::CommandExt as _;
    let rig = Rig::new().await;
    let peer = Peer::new(&rig);
    let mut leader = std::process::Command::new("sh")
        .arg("-c")
        .arg("sleep 635 & exec sleep 0.4")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let group = leader.id();
    let _reap = common::GroupGuard(group);
    let start = start_of(group);
    until("the member of the group", || {
        group_members(group).len() >= 2
    })
    .await;
    let member = group_members(group)
        .into_iter()
        .find(|p| *p != group)
        .unwrap();
    leader.wait().unwrap();
    assert!(alive(member), "it outlives its leader");
    let id = adam_llm_agent::output_id(&rig.run(), "orphan");
    let record = json!({
        "command": "orphan", "slot": "remote", "cwd": "", "exec": "bg-orphan-1",
        "owner": "a-worker-that-is-gone", "pid": group, "start": start, "started_at": 1,
        "state": "running", "ran_secs": null, "lines": 0, "git": null, "beat": 0,
            "host": rig.fx.env.commands.host(),
    });
    rig.fx
        .env
        .kept
        .put_record(&rig.run(), &id, &serde_json::to_vec(&record).unwrap())
        .unwrap();
    assert!(peer.list(&rig).await.contains(&format!("{id} gone")));
    until_gone("what the leader left", &[member]).await;
}

// ------------------------------------------------------------------------- several workers (M1)

fn short_sweep(s: &mut CoderSettings) {
    s.background_sweep = Duration::from_millis(200);
}

/// A fresh record of another worker is a live command: listed as running, left alone by a start of
/// another id, readable.
#[tokio::test]
async fn a_fresh_record_of_another_worker_is_listed_running_and_survives_a_start() {
    let rig = Rig::with(short_sweep).await;
    let started = rig
        .start(
            "start-1",
            json!({"command": TREE, "wait_for": "up", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    let leader = rig.pid_of(&id);
    let peer = Peer::new(&rig);

    let listed = peer.list(&rig).await;
    assert!(
        listed.contains(&format!("{id} running")) && listed.contains("(on another worker)"),
        "{listed}"
    );
    assert!(listed.contains("1 running"), "{listed}");
    let other = peer
        .start(
            &rig,
            "other",
            json!({"command": "echo ready; sleep 636", "wait_for": "ready", "wait": 20}),
        )
        .await;
    assert!(!other.is_error, "{}", other.content);
    assert!(alive(leader), "a start of another id leaves it alone");
    assert!(rig.states()[0].1.is_running());
    assert!(
        peer.list(&rig).await.contains("2 running"),
        "both are counted"
    );
    // Its output is the shared file.
    assert!(rig.read(&id, 1).await.starts_with("up\n"));

    let tree = group_members(leader);
    rig.stop(&id).await;
    peer.stop(&rig, &rig.id_of("other")).await;
    until_gone("the tree", &tree).await;
}

/// `stop_command` of a command another worker runs: a request in its record, the owner's sweep stops
/// it, and both agree on the end.
#[tokio::test]
async fn a_stop_from_another_worker_stops_the_command_within_a_sweep() {
    let rig = Rig::with(short_sweep).await;
    let store = open_store(&rig).await;
    let sweeping = Sweeping::of(&rig.fx.env, &store, Duration::from_millis(200));
    let started = rig
        .start(
            "start-1",
            json!({"command": TREE, "wait_for": "up", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    let leader = rig.pid_of(&id);
    until("the tree to be there", || group_members(leader).len() >= 3).await;
    let tree = group_members(leader);

    let peer = Peer::new(&rig);
    let stopped = peer.stop(&rig, &id).await;
    assert!(!stopped.is_error, "{}", stopped.content);
    assert!(
        stopped.content.contains("stopped: stop_command"),
        "{}",
        stopped.content
    );
    until_gone("the tree", &tree).await;
    // The owner and the record say the same.
    assert!(matches!(
        &rig.states()[0].1,
        CommandState::Stopped { why } if why == "stop_command"
    ));
    assert_eq!(record_of(&rig, &id)["state"], "stopped");
    assert_eq!(record_of(&rig, &id)["why"], "stop_command");
    // Asking again for what ended says how it ended.
    assert!(
        peer.stop(&rig, &id)
            .await
            .content
            .contains("stopped: stop_command")
    );
    sweeping.finish().await;
}

/// When the owner does not sweep, the caller is told the stop was asked, and the owner's next sweep
/// does it.
#[tokio::test]
async fn a_stop_asked_of_a_worker_that_has_not_swept_is_done_by_its_next_sweep() {
    let rig = Rig::with(short_sweep).await;
    let store = open_store(&rig).await;
    let started = rig
        .start(
            "start-1",
            json!({"command": "echo up; sleep 637", "wait_for": "up", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    let pid = rig.pid_of(&id);
    let peer = Peer::new(&rig);
    let asked = peer.stop(&rig, &id).await;
    assert!(
        !asked.is_error && asked.content.contains("stop asked"),
        "{}",
        asked.content
    );
    assert!(alive(pid), "nobody has swept yet");
    assert_eq!(record_of(&rig, &id)["stop_requested"], "stop_command");
    assert_eq!(rig.fx.env.commands.sweep(store.as_ref()).await, 1);
    until_gone("the process", &[pid]).await;
    until("it to be settled", || {
        rig.states().iter().all(|(_, s)| !s.is_running())
    })
    .await;
    assert!(matches!(
        &rig.states()[0].1,
        CommandState::Stopped { why } if why == "stop_command"
    ));
}

/// A beat keeps a stop request that another worker wrote.
#[tokio::test]
async fn a_beat_does_not_erase_a_stop_request() {
    let rig = Rig::new().await;
    let started = rig
        .start(
            "start-1",
            json!({"command": "echo up; sleep 1", "wait_for": "up", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    set_record(&rig, &id, |r| {
        r["stop_requested"] = json!("asked elsewhere")
    });
    // It ends by itself, and writes its record: the request is still there.
    until("it to end", || {
        rig.states().iter().all(|(_, s)| !s.is_running())
    })
    .await;
    let record = record_of(&rig, &id);
    assert_eq!(record["stop_requested"], "asked elsewhere", "{record}");
    assert_eq!(record["state"], "exited");
}

/// The sweep writes the beat of what it holds.
#[tokio::test]
async fn the_sweep_writes_the_beat() {
    let rig = Rig::new().await;
    let store = open_store(&rig).await;
    let started = rig
        .start(
            "start-1",
            json!({"command": "echo up; sleep 638", "wait_for": "up", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    set_record(&rig, &id, |r| r["beat"] = json!(now_secs() - 30));
    assert_eq!(rig.fx.env.commands.sweep(store.as_ref()).await, 0);
    let beat = record_of(&rig, &id)["beat"].as_u64().unwrap();
    assert!(beat + 5 >= now_secs(), "{beat} now {}", now_secs());
    rig.stop(&id).await;
}

/// A record that names another owner is not this worker's any more: its sweep stops the command.
#[tokio::test]
async fn the_sweep_stops_a_command_whose_record_another_worker_took() {
    let rig = Rig::new().await;
    let store = open_store(&rig).await;
    let started = rig
        .start(
            "start-1",
            json!({"command": "echo up; sleep 639", "wait_for": "up", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    let pid = rig.pid_of(&id);
    set_record(&rig, &id, |r| r["owner"] = json!("somebody-else"));
    assert_eq!(rig.fx.env.commands.sweep(store.as_ref()).await, 1);
    until_gone("the process", &[pid]).await;
    until("it to be settled", || {
        rig.states().iter().all(|(_, s)| !s.is_running())
    })
    .await;
    assert!(matches!(
        &rig.states()[0].1,
        CommandState::Stopped { why } if why == "another worker took the run over"
    ));
    // What the new owner wrote stays: this worker does not write over it.
    assert_eq!(record_of(&rig, &id)["owner"], "somebody-else");
}

/// A worker that stops stops what it holds, says so, and runs nothing afterwards.
#[tokio::test]
async fn a_worker_that_stops_ends_its_commands_and_runs_nothing_after() {
    let rig = Rig::with(short_sweep).await;
    let store = open_store(&rig).await;
    let sweeping = Sweeping::of(&rig.fx.env, &store, Duration::from_millis(200));
    let started = rig
        .start(
            "start-1",
            json!({"command": TREE, "wait_for": "up", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    let leader = rig.pid_of(&id);
    until("the tree to be there", || group_members(leader).len() >= 3).await;
    let tree = group_members(leader);
    sweeping.finish().await;
    assert!(
        matches!(&rig.states()[0].1, CommandState::Gone { why } if why.starts_with("the worker stopped")),
        "{:?}",
        rig.states()
    );
    until_gone("the tree", &tree).await;
    assert_eq!(record_of(&rig, &id)["state"], "gone");
}

/// A stale record is a dead worker's: gone, its process (and its group) killed.
#[tokio::test]
async fn a_stale_record_is_gone_and_its_whole_group_is_killed() {
    let rig = Rig::new().await;
    let started = rig
        .start(
            "start-1",
            json!({"command": TREE, "wait_for": "up", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    let leader = rig.pid_of(&id);
    until("the tree to be there", || group_members(leader).len() >= 3).await;
    let tree = group_members(leader);
    age(&rig, &id);
    let peer = Peer::new(&rig);
    let listed = peer.list(&rig).await;
    assert!(
        listed.contains(&format!(
            "{id} gone: the worker that ran it is gone; start it again"
        )),
        "{listed}"
    );
    until_gone("the tree", &tree).await;
    assert_eq!(record_of(&rig, &id)["state"], "gone");
    // `stop_command` of it says the same.
    assert!(peer.stop(&rig, &id).await.content.contains("gone:"));
}

/// A start step that runs again on another worker, while the first worker beats, starts nothing.
#[tokio::test]
async fn a_replayed_start_on_another_worker_starts_no_second_process() {
    let rig = Rig::new().await;
    let work = rig.worktree();
    let command = "echo started >> replayed.txt; sleep 641";
    let first = rig
        .start("start-1", json!({"command": command, "wait": 1}))
        .await;
    assert!(!first.is_error, "{}", first.content);
    let id = rig.id_of("start-1");
    let pid = rig.pid_of(&id);
    until("the line", || work.join("replayed.txt").exists()).await;
    let peer = Peer::new(&rig);
    let again = peer
        .start(&rig, "start-1", json!({"command": command, "wait": 1}))
        .await;
    assert!(!again.is_error, "{}", again.content);
    assert!(
        again
            .content
            .contains(&format!("running as {id} on another worker")),
        "{}",
        again.content
    );
    assert_eq!(
        std::fs::read_to_string(work.join("replayed.txt")).unwrap(),
        "started\n",
        "nothing was started twice"
    );
    assert_eq!(rig.pid_of(&id), pid);
    assert!(peer.env.commands.states(&rig.run()).is_empty());
    rig.stop(&id).await;
}

/// The limit counts the commands of the run on every worker.
#[tokio::test]
async fn the_limit_counts_the_commands_other_workers_run() {
    let rig = Rig::with(|s| s.background_commands_max = 2).await;
    for call in ["a", "b"] {
        let out = rig
            .start(
                call,
                json!({"command": "echo up; sleep 642", "wait_for": "up", "wait": 20}),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
    }
    let peer = Peer::new(&rig);
    let refused = peer
        .start(&rig, "c", json!({"command": "sleep 643", "wait": 1}))
        .await;
    assert!(
        refused.is_error && refused.content.contains("at most 2"),
        "{}",
        refused.content
    );
    assert!(peer.env.commands.states(&rig.run()).is_empty());
    for call in ["a", "b"] {
        rig.stop(&rig.id_of(call)).await;
    }
}

/// Two starts at once at one below the maximum: one takes the place, the other is refused, though the
/// first has not started its process yet.
#[tokio::test]
async fn two_starts_at_once_at_one_below_the_maximum_take_one_place() {
    let rig = Rig::with(|s| s.background_commands_max = 2).await;
    let first = rig
        .start(
            "a",
            json!({"command": "echo up; sleep 644", "wait_for": "up", "wait": 20}),
        )
        .await;
    assert!(!first.is_error, "{}", first.content);
    let (b, c) = tokio::join!(
        rig.start(
            "b",
            json!({"command": "echo up; sleep 645", "wait_for": "up", "wait": 20})
        ),
        rig.start(
            "c",
            json!({"command": "echo up; sleep 646", "wait_for": "up", "wait": 20})
        ),
    );
    assert_eq!(
        u8::from(b.is_error) + u8::from(c.is_error),
        1,
        "{}\n---\n{}",
        b.content,
        c.content
    );
    assert_eq!(rig.fx.env.commands.running_count(&rig.run()), 2);
    for call in ["a", "b", "c"] {
        rig.stop(&rig.id_of(call)).await;
    }
}

// ------------------------------------------------------- the run ends or parks on another worker

/// The commands another worker holds are asked to stop by the worker that steps the run, and their
/// owner's sweep does it.
#[tokio::test]
async fn a_park_stepped_by_another_worker_stops_the_command_within_a_sweep() {
    let fx = Fixture::with(short_sweep).await;
    let url = fx.remote_url();
    let stepping = another_env(&fx);
    let driven = Driven::with_env(stepping.clone(), |m| {
        m.push_tool_calls(vec![call(
            "c1",
            "prepare_workspace",
            json!({"repo_url": url, "base_branch": "main"}),
        )]);
        // The owner of the command starts it while this call runs.
        m.push_tool_calls(vec![call(
            "c2",
            "run_command",
            json!({"command": "sleep 3"}),
        )]);
        m.push_tool_calls(vec![call(
            "c3",
            "ask_user",
            json!({"question": "Shall I go on?"}),
        )]);
    });
    let run = driven.start(&format!("look at {url}")).await;
    let run_id = run.to_string();
    until("the workspace", || {
        common::slot_dir(&fx.root, &run_id).join(".git").exists()
    })
    .await;
    let sweeping = Sweeping::of(&fx.env, &driven.store, Duration::from_millis(200));
    let ctx = ToolCtx::detached("tool", "a1", Arc::new(CollectingSink::new()))
        .with_state(fx.env.clone())
        .with_state(Arc::new(KeptStore(fx.env.kept.clone())))
        .with_root_run(run);
    let started = StartCommand
        .call(&ctx, json!({"command": TREE, "wait_for": "up", "wait": 20}))
        .await
        .unwrap();
    assert!(!started.is_error, "{}", started.content);
    let id = adam_llm_agent::output_id_of(&ctx);
    let leader = {
        let bytes = fx.env.kept.get_record(&run_id, &id).unwrap().unwrap();
        let record: Value = serde_json::from_slice(&bytes).unwrap();
        u32::try_from(record["pid"].as_u64().unwrap()).unwrap()
    };
    until("the tree", || group_members(leader).len() >= 3).await;
    let tree = group_members(leader);

    assert_eq!(driven.settle(run).await, RunStatus::Parked);
    until_gone("the tree", &tree).await;
    until("it to be settled", || stopped_why(&fx, run).len() == 1).await;
    assert_eq!(
        stopped_why(&fx, run),
        ["the run waits for the person's answer"]
    );
    assert!(
        stepping.commands.states(&run_id).is_empty(),
        "the worker that stepped the run held nothing"
    );
    sweeping.finish().await;
    driven.finish().await;
}

/// A run that fails ends its commands too.
#[tokio::test]
async fn a_root_run_that_fails_stops_its_commands_in_the_step() {
    let fx = Fixture::new().await;
    let url = fx.remote_url();
    let driven = Driven::new(&fx, |m| {
        m.push_tool_calls(vec![call(
            "c1",
            "prepare_workspace",
            json!({"repo_url": url, "base_branch": "main"}),
        )]);
        m.push_tool_calls(vec![call(
            "c2",
            "start_command",
            json!({"command": "echo up; sleep 648", "wait_for": "up", "wait": 20}),
        )]);
        m.push_error(ModelError::invalid_request(
            "the provider refuses the request",
        ));
    });
    let run = driven.start(&format!("look at {url}")).await;
    assert_eq!(driven.settle(run).await, RunStatus::Failed);
    assert_eq!(stopped_why(&fx, run), ["the run ended"]);
    assert_eq!(fx.env.commands.running_count(&run.to_string()), 0);
    driven.finish().await;
}

// ------------------------------------------------------------------------------ the output budget

/// The run's own budget is kept while a command streams.
#[tokio::test]
async fn output_that_streams_past_the_runs_budget_stops_with_the_line_that_says_so() {
    let rig = Rig::with(|s| s.run_kept_max = 64 * 1024).await;
    let started = rig
        .start(
            "start-1",
            json!({"command": "seq 1 100000; echo end; sleep 650", "wait_for": "end", "wait": 30}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    rig.stop(&id).await;
    let size = std::fs::metadata(rig.file_of(&id)).unwrap().len();
    assert!(
        (60 * 1024..=80 * 1024).contains(&size),
        "the file stops at the run's budget, to within one read: {size}"
    );
    let page = rig.read(&id, -3).await;
    assert!(
        page.contains("(output cut: a run keeps at most 64 KiB of output)"),
        "{page}"
    );
}

/// A run with no room left refuses the next command before anything is spawned.
#[tokio::test]
async fn a_start_in_a_run_with_no_room_left_is_refused_before_anything_is_spawned() {
    let rig = Rig::with(|s| s.run_kept_max = 64 * 1024).await;
    let first = rig
        .start(
            "a",
            json!({"command": "seq 1 11000; echo end; sleep 651", "wait_for": "end", "wait": 30}),
        )
        .await;
    assert!(!first.is_error, "{}", first.content);
    let marker = rig.worktree().join("spawned.txt");
    let refused = rig
        .start(
            "b",
            json!({"command": "echo spawned > spawned.txt; sleep 652", "wait": 1}),
        )
        .await;
    assert!(
        refused.is_error && refused.content.contains("Nothing was started"),
        "{}",
        refused.content
    );
    assert!(!marker.exists(), "no process was spawned");
    assert_eq!(rig.fx.env.commands.running_count(&rig.run()), 1);
    rig.stop(&rig.id_of("a")).await;
}

// ------------------------------------------------------------------------------------- the rest

#[tokio::test]
async fn what_a_command_writes_to_stderr_is_kept_too() {
    let rig = Rig::new().await;
    let out = rig
        .start(
            "start-1",
            json!({"command": "echo to-stdout; echo to-stderr >&2; sleep 653", "wait_for": "to-stderr", "wait": 20}),
        )
        .await;
    assert!(out.content.contains("to-stderr"), "{}", out.content);
    let id = rig.id_of("start-1");
    let page = rig.read(&id, 1).await;
    assert!(
        page.contains("to-stdout") && page.contains("to-stderr"),
        "{page}"
    );
    rig.stop(&id).await;
}

/// A start whose future is dropped while it tells about it reserves nothing.
#[tokio::test]
async fn a_start_that_is_dropped_before_it_has_a_supervisor_leaves_nothing_behind() {
    struct Blocking(Arc<tokio::sync::Notify>);
    #[async_trait::async_trait]
    impl adam_runtime::EventSink for Blocking {
        async fn emit(&self, _run: RunId, _agent: &str, _event: adam_runtime::RunEvent) {
            self.0.notify_one();
            std::future::pending::<()>().await;
        }
    }
    let rig = Rig::with(|s| s.background_commands_max = 1).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let ctx = ToolCtx::detached("tool", "blocked", Arc::new(Blocking(entered.clone())))
        .with_state(rig.fx.env.clone())
        .with_state(Arc::new(KeptStore(rig.fx.env.kept.clone())))
        .with_root_run(rig.ctx.run_id());
    let id = adam_llm_agent::output_id_of(&ctx);
    {
        let call = StartCommand.call(&ctx, json!({"command": "sleep 654", "wait": 1}));
        tokio::select! {
            _ = call => panic!("the call cannot finish"),
            () = entered.notified() => {}
        }
        // The future is dropped here, while it is telling about what it starts.
    }
    assert!(rig.states().is_empty(), "{:?}", rig.states());
    assert!(!rig.fx.env.kept.is_live(&rig.run(), &id));
    // The limit is free.
    let out = rig
        .start("next", json!({"command": "sleep 655", "wait": 1}))
        .await;
    assert!(!out.is_error, "{}", out.content);
    rig.stop(&rig.id_of("next")).await;
}

/// A command in a run pod that a signal ended and nobody asked: the model is told the
/// likely reason. In this container the same signal is only a signal.
#[tokio::test]
async fn a_signal_nobody_asked_for_says_what_the_likely_reason_is_in_a_pod() {
    let fake = FakeEnvironment::new(None);
    // The session is a run pod's: a signal that nobody asked for is not the caller's kill.
    fake.session.as_pod(None, None);
    let rig = Rig::from(Fixture::new().await.using(fake.clone())).await;
    let out = rig
        .start(
            "start-1",
            json!({"command": "echo up; sleep 657", "wait_for": "up", "wait": 20}),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    let id = rig.id_of("start-1");
    let pid = rig.pid_of(&id);
    kill(pid);
    until("it to end", || {
        rig.states().iter().all(|(_, s)| !s.is_running())
    })
    .await;
    let listed = rig.list().await;
    assert!(
        listed.contains(&format!(
            "{id} stopped: its environment was made again (a new repository or a reference)"
        )) && listed.contains("start it again if it is still needed"),
        "{listed}"
    );

    let local = Rig::new().await;
    let out = local
        .start(
            "start-1",
            json!({"command": "echo up; sleep 658", "wait_for": "up", "wait": 20}),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    let id = local.id_of("start-1");
    kill(local.pid_of(&id));
    until("it to end", || {
        local.states().iter().all(|(_, s)| !s.is_running())
    })
    .await;
    assert!(
        local
            .list()
            .await
            .contains(&format!("{id} ended by a signal")),
        "{}",
        local.list().await
    );
}

fn kill(pid: u32) {
    let status = std::process::Command::new("kill")
        .args(["-s", "KILL", "--"])
        .arg(format!("-{pid}"))
        .status()
        .unwrap();
    assert!(status.success());
}

/// The time a command ran is metered once, at the call that sees its end.
#[tokio::test]
async fn the_time_of_a_command_is_metered_once_at_the_call_that_sees_its_end() {
    let fake = FakeEnvironment::new(None);
    fake.session.as_pod(Some(250), Some(512 * 1024 * 1024));
    let rig = Rig::from(Fixture::new().await.using(fake)).await;
    let started = rig
        .start(
            "start-1",
            json!({"command": "echo up; sleep 659", "wait_for": "up", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    assert!(
        rig.call_ctx("start-1")
            .reported_sandbox()
            .entries()
            .is_empty(),
        "a command that runs is not metered yet"
    );
    let id = rig.id_of("start-1");
    rig.stop(&id).await;
    let at_stop = rig.call_ctx("stop").reported_sandbox();
    let [entry] = at_stop.entries() else {
        panic!("one entry at the call that sees the end: {at_stop:?}");
    };
    assert_eq!(entry.commands, 1);
    // Later calls that see the same end report nothing more.
    rig.list().await;
    rig.stop(&id).await;
    assert!(rig.call_ctx("list").reported_sandbox().entries().is_empty());
    assert_eq!(
        rig.call_ctx("stop").reported_sandbox().entries()[0].commands,
        1
    );
    // A command that ends inside its wait is metered at the call that started it.
    let quick = rig
        .start("quick", json!({"command": "echo hi", "wait": 20}))
        .await;
    assert!(quick.content.contains("exit code 0"), "{}", quick.content);
    assert_eq!(
        rig.call_ctx("quick").reported_sandbox().entries()[0].commands,
        1
    );
    rig.list().await;
    assert!(rig.call_ctx("list").reported_sandbox().entries().is_empty());
}

// ------------------------------------------------------------------------------ the wiring

/// The tools, the janitor and the sweep of a worker share one registry, and the sweep runs at the
/// interval the deployment configured.
#[tokio::test]
async fn a_worker_wires_one_registry_to_the_tools_the_janitor_and_the_sweep() {
    let fx = Fixture::new().await;
    let vars = [
        ("DATABASE_URL", "postgres://u:p@db/adam"),
        ("MODEL_BASE_URL", "https://gw.example/v1"),
        ("MODEL_API_KEY", "sk-secret"),
        ("MODEL", "coder-large"),
        ("GITHUB_TOKEN", "ghp_secret"),
        ("A2A_BEARER_TOKENS", "one"),
        ("PUBLIC_URL", "http://coder.svc:8080/"),
        ("BACKGROUND_SWEEP_SECS", "7"),
    ];
    let config = adam_coder::Config::from_lookup(|k| {
        vars.iter()
            .find(|(name, _)| *name == k)
            .map(|(_, v)| (*v).to_owned())
    })
    .unwrap();
    let worker = config.worker.expect("a worker");
    let janitor = adam_coder::Janitor::new(fx.env.workspaces.clone(), None);
    assert_ne!(janitor.commands().owner(), fx.env.commands.owner());
    let (janitor, sweep) = adam_coder::wire_commands(&fx.env, janitor, &worker);
    assert_eq!(janitor.commands().owner(), fx.env.commands.owner());
    assert_eq!(sweep.commands().owner(), fx.env.commands.owner());
    assert_eq!(sweep.every(), Duration::from_secs(7));
}

/// The component the worker runs stops the commands of a cancelled run within its interval, with
/// nobody calling the agent.
#[tokio::test]
async fn the_sweep_component_stops_the_commands_of_a_cancelled_run_within_its_interval() {
    let rig = Rig::with(short_sweep).await;
    let store = open_store(&rig).await;
    let started = rig
        .start(
            "start-1",
            json!({"command": TREE, "wait_for": "up", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    let leader = rig.pid_of(&id);
    until("the tree", || group_members(leader).len() >= 3).await;
    let tree = group_members(leader);
    let sweep = adam_coder::CommandsSweep::new(&rig.fx.env.commands, Duration::from_millis(200));
    let token = CancellationToken::new();
    let handle = tokio::spawn(sweep.run(store.clone(), token.clone()));
    // Still open: nothing is stopped by looking.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(alive(leader));
    // A cancel commits `Failed` and never calls the agent.
    let record = store.load_run(rig.ctx.run_id()).await.unwrap().unwrap();
    store
        .commit_run(
            rig.ctx.run_id(),
            record.version,
            adam_core::RunUpdate::new(RunStatus::Failed, record.state),
        )
        .await
        .unwrap();
    until_gone("the tree", &tree).await;
    until("it to be settled", || {
        rig.states().iter().all(|(_, s)| !s.is_running())
    })
    .await;
    assert!(matches!(
        &rig.states()[0].1,
        CommandState::Stopped { why } if why == "the run ended"
    ));
    token.cancel();
    handle.await.unwrap().unwrap();
}

/// A registry that goes (a worker that ends, a test that forgot to stop) takes what it holds with
/// it: the groups, not only the leaders.
#[tokio::test]
async fn dropping_the_registry_kills_the_groups_it_holds() {
    let rig = Rig::new().await;
    let started = rig
        .start(
            "start-1",
            json!({"command": TREE, "wait_for": "up", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let leader = rig.pid_of(&rig.id_of("start-1"));
    until("the tree", || group_members(leader).len() >= 3).await;
    let tree = group_members(leader);
    drop(rig);
    until_gone("the tree", &tree).await;
}

// ------------------------------------------------------------------------------- the host

/// A record of a stale command that another machine started: a pid and a start time that match a live
/// process of this machine do not make it the record's. Nothing local is signalled, the exec id is
/// killed where there is a session, and the record says `gone`. With this machine's host the same
/// record kills the process.
#[tokio::test]
async fn a_pid_is_only_killed_on_the_machine_that_the_record_names() {
    use std::os::unix::process::CommandExt as _;
    let fake = FakeEnvironment::new(None);
    let rig = Rig::from(Fixture::new().await.using(fake.clone())).await;
    let peer = Peer {
        env: {
            let mut env = Arc::try_unwrap(another_worker(&rig))
                .ok()
                .expect("not shared");
            env.environment = fake.clone();
            Arc::new(env)
        },
    };
    let spawn = |seconds: &str| {
        std::process::Command::new("sleep")
            .arg(seconds)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    };
    let mut elsewhere = spawn("663");
    let mut nowhere = spawn("664");
    let mut here = spawn("665");
    let _reap = (
        common::GroupGuard(elsewhere.id()),
        common::GroupGuard(nowhere.id()),
        common::GroupGuard(here.id()),
    );
    let this_host = rig.fx.env.commands.host().to_owned();
    let mut ids = Vec::new();
    for (name, child, host) in [
        ("elsewhere", &elsewhere, Some("another-pod".to_owned())),
        ("nowhere", &nowhere, None),
        ("here", &here, Some(this_host)),
    ] {
        let id = adam_llm_agent::output_id(&rig.run(), name);
        let mut record = json!({
            "command": name, "slot": "remote", "cwd": "", "exec": format!("bg-host-{name}"),
            "owner": "a-worker-that-is-gone", "pid": child.id(), "start": start_of(child.id()),
            "started_at": 1, "state": "running", "ran_secs": null, "lines": 0, "git": null,
            "beat": 0,
        });
        if let Some(host) = host {
            record["host"] = json!(host);
        }
        rig.fx
            .env
            .kept
            .put_record(&rig.run(), &id, &serde_json::to_vec(&record).unwrap())
            .unwrap();
        ids.push(id);
    }
    let listed = peer.list(&rig).await;
    for id in &ids {
        assert!(listed.contains(&format!("{id} gone")), "{listed}");
        assert_eq!(record_of(&rig, id)["state"], "gone");
    }
    until("the process of this machine to be killed", || {
        here.try_wait().unwrap().is_some()
    })
    .await;
    assert!(
        elsewhere.try_wait().unwrap().is_none(),
        "another machine's pid"
    );
    assert!(
        nowhere.try_wait().unwrap().is_none(),
        "a record with no host"
    );
    // The exec id is killed through the environment all the same.
    let killed = fake.session.killed.lock().unwrap().clone();
    for name in ["elsewhere", "nowhere", "here"] {
        assert!(
            killed
                .iter()
                .any(|e| e.as_str() == format!("bg-host-{name}")),
            "{name}: {killed:?}"
        );
    }
    elsewhere.kill().unwrap();
    nowhere.kill().unwrap();
    elsewhere.wait().unwrap();
    nowhere.wait().unwrap();
}
