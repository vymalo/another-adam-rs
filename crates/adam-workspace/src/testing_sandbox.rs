//! A real git sandbox server for the tests of crates that use this one (feature `test-util`).
//!
//! [`GitSandboxServer`] starts the `adam-git-sandbox` binary as a process, on a socket in a directory
//! of the test, with the work volume `root`, and keeps its log. The binary is built on first use with
//! `cargo build -p adam-git-sandbox` in the target directory and the profile of the running test
//! binary (`CARGO_BIN_EXE_*` exists only in the package that owns a binary). The server runs with an
//! environment of its own (`PATH` and its three variables), so a program it runs sees the server's
//! surroundings and not the test's: that is the point of using a process.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// A variable of the server's own environment, with this value: what a program that runs in the
/// sandbox can read of the server, and that the test's own environment does not have.
pub const SANDBOX_MARKER: (&str, &str) = ("SANDBOX_SIDE", "the-sandboxs-own-variable");

/// One command the server ran, as its log says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RanCommand {
    /// The git subcommand.
    pub subcommand: String,
    /// The directory it ran in.
    pub cwd: PathBuf,
    /// How it ended: `exit 0`, `signal 9`, `refused: ...`.
    pub status: String,
}

/// The server as a process. Killed when dropped.
#[derive(Debug)]
pub struct GitSandboxServer {
    child: Child,
    socket: PathBuf,
    log: PathBuf,
}

/// The server's binary, built on first use.
///
/// # Panics
///
/// When cargo cannot build it (this is test support).
#[allow(clippy::expect_used)]
fn binary() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let exe = std::env::current_exe().expect("test executable path");
        let profile_dir = exe
            .parent()
            .and_then(Path::parent)
            .expect("<target>/<profile>/deps/<test>")
            .to_path_buf();
        let profile = profile_dir
            .file_name()
            .and_then(|n| n.to_str())
            .expect("profile directory name")
            .to_owned();
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let mut cmd = Command::new(cargo);
        cmd.args([
            "build",
            "--quiet",
            "--locked",
            "-p",
            "adam-git-sandbox",
            "--bin",
            "adam-git-sandbox",
        ]);
        match profile.as_str() {
            "debug" => {}
            "release" => {
                cmd.arg("--release");
            }
            other => {
                cmd.args(["--profile", other]);
            }
        }
        // The same target directory as this test binary, wherever it is.
        cmd.env(
            "CARGO_TARGET_DIR",
            profile_dir.parent().expect("target dir"),
        );
        let status = cmd.status().expect("cargo runs");
        assert!(status.success(), "building adam-git-sandbox failed");
        let bin = profile_dir.join("adam-git-sandbox");
        assert!(bin.is_file(), "{} was not built", bin.display());
        bin
    })
}

impl GitSandboxServer {
    /// Start a server whose socket and log are in `dir` and whose work volume is `root` (an
    /// existing directory), and wait until it listens.
    ///
    /// # Panics
    ///
    /// When it cannot be started (this is test support).
    #[allow(clippy::expect_used, clippy::unwrap_used)]
    pub fn start(dir: &Path, root: &Path) -> Self {
        let socket = dir.join("git.sock");
        let log = dir.join("git-sandbox.log");
        let child = Command::new(binary())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("PATH is set"))
            .env("GIT_SANDBOX_LISTEN", &socket)
            .env("GIT_SANDBOX_ROOT", root)
            .env(SANDBOX_MARKER.0, SANDBOX_MARKER.1)
            .stdout(std::fs::File::create(&log).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .expect("the server starts");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !socket.exists() {
            assert!(Instant::now() < deadline, "the server made no socket");
            std::thread::sleep(Duration::from_millis(20));
        }
        Self { child, socket, log }
    }

    /// The socket the server listens on.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The server's process id: the ancestor a program it runs finds.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The text of the server's log.
    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Every command the server ran so far, in order. A command's line is written before the caller
    /// is told it has ended.
    pub fn commands(&self) -> Vec<RanCommand> {
        self.log()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|line| line["fields"]["message"] == "git command")
            .map(|line| {
                let field = |name: &str| line["fields"][name].as_str().unwrap_or("").to_owned();
                RanCommand {
                    subcommand: field("subcommand"),
                    cwd: PathBuf::from(field("cwd")),
                    status: field("status"),
                }
            })
            .collect()
    }

    /// The git subcommand of every command the server ran so far, in order.
    pub fn subcommands(&self) -> Vec<String> {
        self.commands().into_iter().map(|c| c.subcommand).collect()
    }
}

impl Drop for GitSandboxServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
