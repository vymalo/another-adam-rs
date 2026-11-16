//! Commands that keep running (ADR 0044, slice 16): `start_command`, `stop_command`,
//! `list_commands`, read with `read_output`, and gone with their run.
//!
//! The commands are real processes (`sh` in this container); the tests start them with short
//! lifetimes and sweeps through the settings, and every test stops what it starts (`sleep 600` is
//! only ever waiting to be stopped).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_coder::tools::background::{CommandState, ListCommands, StartCommand, StopCommand};
use adam_coder::tools::named::{named_in, without_untrusted};
use adam_coder::tools::prepare::PrepareWorkspace;
use adam_coder::{AGENT_NAME, CoderAgent, CoderSettings, Redactor, ToolEnv};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{KeptStore, ReadOutput, Tool, ToolCtx, ToolOutput};
use adam_model::{DynModel, MockModel, ModelRequest};
use adam_runtime::{CollectingSink, Inbound, Runtime};
use common::{FakeEnvironment, Fixture, MODEL_KEY, call};
use serde_json::{Value, json};

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
    let first = &rig.fx.env;
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

    // The worker restarted and the step runs again: another owner, the same call.
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
            "{id} gone: the worker that ran it restarted; start it again"
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

#[tokio::test]
async fn git_changes_of_a_background_command_are_reported_at_stop() {
    let rig = Rig::new().await;
    let started = rig
        .start(
            "start-1",
            json!({"command": "git config user.nickname watcher; echo ready; sleep 600", "wait_for": "ready", "wait": 20}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let id = rig.id_of("start-1");
    let stopped = rig.stop(&id).await;
    assert!(
        stopped.content.contains("git was changed while it ran"),
        "{}",
        stopped.content
    );
    assert!(rig.list().await.contains("(git changed while it ran)"));
    // The files it wrote stay, and the guard of `run_command` names what runs in its slot.
    let second = rig
        .start(
            "start-2",
            json!({"command": "echo ready; sleep 600", "wait_for": "ready", "wait": 20}),
        )
        .await;
    assert!(!second.is_error, "{}", second.content);
    let refused = adam_coder::tools::inspect::RunCommand
        .call(
            &rig.call_ctx("look"),
            json!({"command": "echo changed > README.md"}),
        )
        .await
        .unwrap();
    assert!(refused.is_error, "{}", refused.content);
    assert!(
        refused
            .content
            .contains("Commands that keep running are writing in this slot")
            && refused.content.contains(&rig.id_of("start-2")),
        "{}",
        refused.content
    );
    rig.stop(&rig.id_of("start-2")).await;
}

#[tokio::test]
async fn a_command_is_never_started_in_a_reference() {
    let rig = Rig::new().await;
    // A reference is a slot the run may not write: its name is refused before anything is started.
    let out = rig
        .start(
            "start-1",
            json!({"command": "sleep 600", "repo": "no-such-slot"}),
        )
        .await;
    assert!(out.is_error, "{}", out.content);
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
        let mock = Arc::new(MockModel::new());
        script(&mock);
        let model: DynModel = mock.clone();
        let agent = CoderAgent::new(model, "test-model", fx.env.clone());
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
    assert!(
        matches!(status, RunStatus::Done | RunStatus::Parked),
        "{status:?}"
    );
    // At once, in the agent's step: nothing waited for the sweep.
    let why = stopped_why(&fx, run);
    assert_eq!(why.len(), 1, "{why:?}");
    assert!(
        why[0] == "the run ended" || why[0].contains("waits for the person"),
        "{why:?}"
    );
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
