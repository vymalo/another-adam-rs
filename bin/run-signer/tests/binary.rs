//! The `run-signer` binary as a process: what it does with a bad environment (exit 78, a line
//! naming the variables), a port it cannot bind (71), and what it never writes (the token).
//!
//! These are the acceptance cases no library test can make: the binary's own exit codes, and the
//! guarantee that a configuration it rejects is reported by the process, not by a function a test
//! calls.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::io::{BufRead as _, BufReader, Read as _};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::Value;

/// The sentinel in the token file: a log line that carries the token would carry this.
const SENTINEL: &str = "SENTINEL-do-not-log";

fn command(env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_run-signer"));
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// The `fields` of the one `run-signer failed` line on stdout.
fn failure(stdout: &str) -> Value {
    let lines: Vec<Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["fields"]["message"] == "run-signer failed")
        .collect();
    assert_eq!(lines.len(), 1, "exactly one failure line:\n{stdout}");
    assert_eq!(lines[0]["level"], "ERROR");
    lines[0]["fields"].clone()
}

/// Run to its end with `env`, which must be a refusal to start: its exit code and its message.
fn refused(env: &[(&str, &str)]) -> (i32, String) {
    let out = command(env).output().expect("run-signer runs");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.stderr.is_empty(),
        "nothing goes to stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let fields = failure(&stdout);
    let code = out.status.code().expect("it exited");
    assert_eq!(fields["code"], code, "{stdout}");
    (code, fields["error"].as_str().unwrap().to_owned())
}

/// A token file holding `contents`, and its path.
fn token_file(dir: &Path, contents: &str) -> String {
    let path = dir.join("token");
    std::fs::write(&path, contents).unwrap();
    path.to_str().unwrap().to_owned()
}

/// The certificate half and the upstream, all valid: what every case below varies one of.
fn valid(token: &str) -> Vec<(&str, &str)> {
    vec![
        ("RUN_CERT_ISSUER", "broker"),
        ("RUN_CERT_ISSUER_URL", "https://broker.internal:8443"),
        ("RUN_CERT_TOKEN_FILE", token),
        ("RUN_SIGNER_UPSTREAM", "http://127.0.0.1:1"),
    ]
}

/// The acceptance: an empty environment exits 78, and the one line names **both** halves' variables.
#[test]
fn an_empty_environment_exits_78_naming_both_halves() {
    let (code, message) = refused(&[]);
    assert_eq!(code, 78, "{message}");
    assert!(message.contains("invalid configuration"), "{message}");
    for name in [
        "RUN_CERT_ISSUER",
        "RUN_CERT_ISSUER_URL",
        "RUN_CERT_TOKEN_FILE",
        "RUN_SIGNER_UPSTREAM",
    ] {
        assert!(message.contains(name), "{name} is named by none: {message}");
    }
}

/// The acceptance: a non-loopback bind is refused by the process, naming the variable.
#[test]
fn a_non_loopback_listen_exits_78() {
    let dir = tempfile::tempdir().unwrap();
    let token = token_file(dir.path(), "a token");
    let mut env = valid(&token);
    env.push(("RUN_SIGNER_LISTEN", "0.0.0.0:8443"));
    let (code, message) = refused(&env);
    assert_eq!(code, 78, "{message}");
    assert!(message.contains("RUN_SIGNER_LISTEN"), "{message}");
    assert!(message.contains("loopback"), "{message}");
}

/// A port another process holds: 71, naming the variable and the address.
#[test]
fn a_port_that_cannot_be_bound_exits_71() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let dir = tempfile::tempdir().unwrap();
    let token = token_file(dir.path(), "a token");
    let mut env = valid(&token);
    env.push(("RUN_SIGNER_LISTEN", &addr));
    let (code, message) = refused(&env);
    assert_eq!(code, 71, "{message}");
    assert!(
        message.contains(&format!("cannot listen on {addr}")),
        "{message}"
    );
    assert!(message.contains("RUN_SIGNER_LISTEN"), "{message}");
}

/// A running sidecar, its lines, and the process.
struct Running {
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
}

impl Running {
    /// Start with `env` and wait until it listens.
    fn start(env: &[(&str, &str)]) -> Self {
        let mut child = command(env).spawn().expect("run-signer starts");
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let mut running = Self {
            child,
            lines,
            seen: Vec::new(),
        };
        running.wait_for(|fields| fields["message"] == "adam-run-signer listening");
        running
    }

    /// The `fields` of the first line (from the start) that `wanted` accepts, waiting for it.
    fn wait_for(&mut self, wanted: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut at = 0;
        loop {
            while at < self.seen.len() {
                let line = &self.seen[at];
                at += 1;
                if let Ok(json) = serde_json::from_str::<Value>(line)
                    && wanted(&json["fields"])
                {
                    return json["fields"].clone();
                }
            }
            match self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(line) => self.seen.push(line),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    panic!("the line never came; logs:\n{}", self.seen.join("\n"))
                }
            }
        }
    }

    fn sigterm(&self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill runs");
        assert!(status.success());
    }

    /// The exit code, once it is gone, and both streams' text.
    fn finish(&mut self) -> (Option<i32>, String) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "still running:\n{}",
                self.seen.join("\n")
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        // Drain what the reader has left.
        while let Ok(line) = self.lines.recv_timeout(Duration::from_millis(100)) {
            self.seen.push(line);
        }
        (
            self.child.try_wait().unwrap().and_then(|s| s.code()),
            stderr,
        )
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The acceptance: the token file's content reaches no line of stdout or stderr, and a clean SIGTERM
/// is exit 0.
#[test]
fn the_token_is_on_no_line() {
    let dir = tempfile::tempdir().unwrap();
    let token = token_file(dir.path(), SENTINEL);
    let mut env = valid(&token);
    env.push(("RUN_SIGNER_LISTEN", "127.0.0.1:0"));

    let mut running = Running::start(&env);
    running.sigterm();
    let (code, stderr) = running.finish();
    assert_eq!(code, Some(0), "{:?}", running.seen.join("\n"));
    assert!(
        !running.seen.iter().any(|line| line.contains(SENTINEL)),
        "the token was logged:\n{}",
        running.seen.join("\n")
    );
    assert!(!stderr.contains(SENTINEL), "{stderr}");
}
