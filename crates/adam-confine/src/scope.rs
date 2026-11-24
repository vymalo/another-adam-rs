//! One transient systemd scope per command.

use std::process::{Command as StdCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use adam_workspace::{
    Confine, ConfineError, Confined, Confinement, Ended, Enforcement, ProcessLimits, Stopped,
};
use async_trait::async_trait;
use tokio::process::Command;

use crate::child;
use crate::rlimits::{is_shell, plan, stopped_by_rlimit};

/// How long the probe at start may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a `systemctl` call of a command's end may take.
const CTL_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the end of a scope is awaited: systemd settles its result after the process is gone.
const SETTLE: Duration = Duration::from_secs(2);

/// The slice every scope of adam's commands is made under.
const SLICE: &str = "adam-commands";

static NEXT: AtomicU64 = AtomicU64::new(1);

/// One transient scope per command, made through the person's own systemd user manager (no
/// administrator rights): `systemd-run --user --scope -p MemoryMax=... -p CPUQuota=... -p
/// TasksMax=...`. A command that passes its memory is killed by the kernel and the scope says
/// `oom-kill` ([`Stopped::Memory`]); a flood of processes meets `TasksMax` and its forks fail.
/// [`stop`](Confinement::stop) kills the whole scope, which a process that left its process group
/// cannot escape.
///
/// [`detect`](Self::detect) asks the system what it enforces, because a distribution may not
/// delegate every controller to the user manager.
#[derive(Debug, Clone, Copy)]
pub struct SystemdScope {
    memory: bool,
    cpu: bool,
    tasks: bool,
}

impl SystemdScope {
    /// Probe the system: make a scope with the three limits and read them back from the cgroup the
    /// scope made. `None` when no scope can be made (no systemd, no user manager, no cgroup v2).
    /// Blocks for up to `PROBE_TIMEOUT`; call it once, at start.
    #[must_use]
    pub fn detect() -> Option<Self> {
        let script = "d=/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup); \
                      echo mem=$(cat $d/memory.max 2>/dev/null); \
                      echo pids=$(cat $d/pids.max 2>/dev/null); \
                      echo cpu=$(cat $d/cpu.max 2>/dev/null)";
        let mut child = StdCommand::new("systemd-run")
            .args(["--user", "--scope", "--quiet"])
            .args([
                "-p",
                "MemoryMax=64M",
                "-p",
                "TasksMax=64",
                "-p",
                "CPUQuota=50%",
            ])
            .args(["--", "sh", "-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() < PROBE_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        };
        if !status.success() {
            return None;
        }
        let mut text = String::new();
        if let Some(mut out) = child.stdout.take() {
            std::io::Read::read_to_string(&mut out, &mut text).ok()?;
        }
        Some(Self {
            memory: text.lines().any(|l| l == "mem=67108864"),
            tasks: text.lines().any(|l| l == "pids=64"),
            cpu: text.lines().any(|l| l.starts_with("cpu=50000 ")),
        })
    }

    /// A scope backend that enforces everything, for a test that cannot probe.
    #[doc(hidden)]
    #[must_use]
    pub fn assume_full() -> Self {
        Self {
            memory: true,
            cpu: true,
            tasks: true,
        }
    }

    /// Whether the memory controller is delegated: the limit that matters most.
    #[must_use]
    pub fn enforces_memory(&self) -> bool {
        self.memory
    }

    pub(crate) fn describe_missing(&self) -> String {
        match self.describe() {
            Enforcement::Partial(missing) => missing.join(", "),
            _ => String::new(),
        }
    }
}

struct Held {
    unit: String,
    via_shell: bool,
}

impl Held {
    async fn ctl(&self, args: &[&str]) -> Option<String> {
        let run = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        let out = tokio::time::timeout(CTL_TIMEOUT, run).await.ok()?.ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }
}

impl Held {
    /// The `Result` of the scope once it is no longer active, which systemd sets a moment after the
    /// process ends (`oom-kill` among them); whatever it says at the cap.
    async fn settled_result(&self, unit: &str) -> Option<String> {
        let deadline = Instant::now() + SETTLE;
        loop {
            let shown = self
                .ctl(&["show", unit, "-p", "ActiveState", "-p", "Result"])
                .await;
            let field = |name: &str| {
                shown.as_deref().and_then(|text| {
                    text.lines()
                        .find_map(|l| l.strip_prefix(name)?.strip_prefix('='))
                        .map(str::to_owned)
                })
            };
            let settled = matches!(field("ActiveState").as_deref(), Some("inactive" | "failed"));
            if settled || shown.is_none() || Instant::now() >= deadline {
                return field("Result");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[async_trait]
impl Confinement for Held {
    async fn stop(&self) {
        let unit = format!("{}.scope", self.unit);
        self.ctl(&["kill", "--signal=SIGKILL", "--kill-whom=all", &unit])
            .await;
    }

    async fn finish(&self, limits: &ProcessLimits, ended: Ended) -> Option<Stopped> {
        let unit = format!("{}.scope", self.unit);
        let result = self.settled_result(&unit).await;
        // A scope that failed stays loaded until it is reset: it must not pile up.
        self.ctl(&["reset-failed", &unit]).await;
        if result.as_deref() == Some("oom-kill")
            && let Some(limit) = limits.memory
        {
            return Some(Stopped::Memory { limit });
        }
        stopped_by_rlimit(limits, ended, self.via_shell)
    }
}

impl Confine for SystemdScope {
    fn confine(&self, cmd: &mut Command, limits: &ProcessLimits) -> Result<Confined, ConfineError> {
        let unit = format!(
            "adam-cmd-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let std: &StdCommand = cmd.as_std();
        let via_shell = is_shell(std.get_program());
        let cleared = env_cleared(std);
        let program = std.get_program().to_owned();
        let args: Vec<_> = std.get_args().map(ToOwned::to_owned).collect();
        let cwd = std.get_current_dir().map(ToOwned::to_owned);
        let envs: Vec<_> = std
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(ToOwned::to_owned)))
            .collect();

        let mut wrapped = Command::new("systemd-run");
        if cleared {
            // A scope runs in the environment of `systemd-run`; it needs these two to reach the
            // person's manager.
            wrapped.env_clear();
            for name in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
                if let Some(value) = std::env::var_os(name) {
                    wrapped.env(name, value);
                }
            }
        }
        wrapped.args([
            "--user", "--scope", "--quiet", "--unit", &unit, "--slice", SLICE,
        ]);
        if let (Some(bytes), true) = (limits.memory, self.memory) {
            wrapped.arg("-p").arg(format!("MemoryMax={bytes}"));
            wrapped.arg("-p").arg("MemorySwapMax=0");
        }
        if let (Some(millicores), true) = (limits.cpu_millicores, self.cpu) {
            wrapped
                .arg("-p")
                .arg(format!("CPUQuota={}%", (millicores / 10).max(1)));
        }
        if let (Some(n), true) = (limits.max_processes, self.tasks) {
            wrapped.arg("-p").arg(format!("TasksMax={n}"));
        }
        wrapped.arg("--").arg(program).args(args);
        if let Some(cwd) = cwd {
            wrapped.current_dir(cwd);
        }
        for (key, value) in envs {
            match value {
                Some(value) => wrapped.env(key, value),
                None => wrapped.env_remove(key),
            };
        }
        child::install(&mut wrapped, plan(limits, false));
        *cmd = wrapped;
        Ok(Confined::held(*limits, Arc::new(Held { unit, via_shell })))
    }

    fn describe(&self) -> Enforcement {
        let mut missing = Vec::new();
        if !self.memory {
            missing.push("memory");
        }
        if !self.cpu {
            missing.push("cpu share");
        }
        if !self.tasks {
            missing.push("process count");
        }
        if missing.is_empty() {
            Enforcement::Full
        } else {
            Enforcement::Partial(missing)
        }
    }
}

/// Whether `env_clear` was called on `cmd`: `std` has no getter, but its `Debug` output starts with
/// `env -i` (after the directory) for a cleared environment. The conformance case
/// `env_clear_is_kept` fails if that ever changes.
fn env_cleared(cmd: &StdCommand) -> bool {
    let debug = format!("{cmd:?}");
    let rest = match cmd.get_current_dir() {
        Some(dir) => debug
            .strip_prefix(&format!("cd {dir:?} && "))
            .unwrap_or(&debug),
        None => &debug,
    };
    rest.starts_with("env -i ")
}
