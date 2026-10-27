//! The `adam-egress` binary as a process: what it does with a bad environment (exit 78, one log
//! line that names the variable), with a port it cannot bind (71), and what it does when it runs:
//! the real resolver and the standard address filter, a probe, a refusal, and SIGTERM.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::Value;

fn command(env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_adam-egress"));
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// The `fields` of the one `adam-egress failed` line on stdout.
fn failure(stdout: &str) -> Value {
    let lines: Vec<Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["fields"]["message"] == "adam-egress failed")
        .collect();
    assert_eq!(lines.len(), 1, "exactly one failure line:\n{stdout}");
    assert_eq!(lines[0]["level"], "ERROR");
    lines[0]["fields"].clone()
}

/// Run to its end with `env`, which must be a refusal to start: its exit code and its message.
fn refused(env: &[(&str, &str)]) -> (i32, String) {
    let out = command(env).output().expect("adam-egress runs");
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

#[test]
fn without_an_allow_list_it_exits_78_and_says_how_to_make_one() {
    let (code, message) = refused(&[]);
    assert_eq!(code, 78);
    assert!(
        message.starts_with("reading the configuration: invalid configuration:"),
        "{message}"
    );
    assert!(message.contains("no host is allowed"), "{message}");
    assert!(
        message.contains("EGRESS_ALLOW") && message.contains("EGRESS_ALLOW_FILE"),
        "{message}"
    );
}

#[test]
fn each_bad_value_exits_78_naming_what_is_wrong() {
    for (env, wanted) in [
        (
            &[("EGRESS_ALLOW", "*.example.com")][..],
            "\"*.example.com\"",
        ),
        (
            &[("EGRESS_ALLOW", "ok.example, http://bad.example")],
            "\"http://bad.example\"",
        ),
        (
            &[("EGRESS_ALLOW", "ok.example"), ("EGRESS_PORTS", "https")],
            "EGRESS_PORTS",
        ),
        (
            &[
                ("EGRESS_ALLOW", "ok.example"),
                ("EGRESS_DENY_CIDRS", "10.0.0.0/40"),
            ],
            "10.0.0.0/40",
        ),
        (
            &[("EGRESS_ALLOW", "ok.example"), ("EGRESS_IDLE_SECS", "0")],
            "EGRESS_IDLE_SECS",
        ),
        (
            &[
                ("EGRESS_ALLOW", "ok.example"),
                ("EGRESS_MAX_CONNECTIONS", "0"),
            ],
            "EGRESS_MAX_CONNECTIONS",
        ),
        (
            &[("EGRESS_ALLOW", "ok.example"), ("EGRESS_LISTEN", "3128")],
            "EGRESS_LISTEN",
        ),
        (
            &[("EGRESS_ALLOW_FILE", "/nonexistent/allow.txt")],
            "/nonexistent/allow.txt",
        ),
    ] {
        let (code, message) = refused(env);
        assert_eq!(code, 78, "{env:?}: {message}");
        assert!(message.contains(wanted), "{env:?}: {message}");
        assert!(message.contains("invalid configuration"), "{message}");
    }
}

#[test]
fn a_port_that_cannot_be_bound_exits_71() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let (code, message) = refused(&[("EGRESS_ALLOW", "ok.example"), ("EGRESS_LISTEN", &addr)]);
    assert_eq!(code, 71, "{message}");
    assert!(
        message.contains(&format!("cannot listen on {addr}")),
        "{message}"
    );
    assert!(message.contains("EGRESS_LISTEN"), "{message}");
}

// ------------------------------------------------------------------------------------ running

/// A running proxy and the log lines it has written.
struct Running {
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
    addr: SocketAddr,
}

impl Running {
    fn start(env: &[(&str, &str)]) -> Self {
        let mut child = command(env).spawn().expect("adam-egress starts");
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
            addr: "127.0.0.1:0".parse().unwrap(),
        };
        let listening = running.wait_for(|fields| fields["message"] == "listening");
        running.addr = listening["addr"].as_str().unwrap().parse().unwrap();
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

    /// Send `bytes`, read the whole answer.
    fn ask(&self, bytes: &[u8]) -> (u16, String) {
        let mut stream = TcpStream::connect(self.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(bytes).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").expect("a complete answer");
        (
            head.split(' ').nth(1).unwrap().parse().unwrap(),
            body.to_owned(),
        )
    }

    fn sigterm(&self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill runs");
        assert!(status.success());
    }

    /// The exit code, once it is gone.
    fn exit_within(&mut self, limit: Duration) -> Option<i32> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            assert!(
                Instant::now() < deadline,
                "still running:\n{}",
                self.seen.join("\n")
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn it_serves_with_the_real_resolver_and_the_standard_filter_and_stops_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("allow.txt");
    std::fs::write(&file, "# for the test\nlocalhost\n.registry.test\n").unwrap();
    let mut proxy = Running::start(&[
        ("EGRESS_LISTEN", "127.0.0.1:0"),
        ("EGRESS_ALLOW_FILE", file.to_str().unwrap()),
        ("EGRESS_ALLOW", "extra.test"),
        ("EGRESS_PORTS", "443,8443"),
        ("EGRESS_DENY_CIDRS", "10.96.0.0/12"),
        ("EGRESS_IDLE_SECS", "30"),
        ("EGRESS_MAX_CONNECTIONS", "8"),
    ]);
    let starting = proxy.wait_for(|fields| fields["message"] == "starting adam-egress");
    let config = starting["config"].as_str().unwrap();
    assert!(
        config.contains("localhost")
            && config.contains(".registry.test")
            && config.contains("extra.test"),
        "{config}"
    );
    let listening = proxy.wait_for(|fields| fields["message"] == "listening");
    assert_eq!(listening["allowed"], 3);
    assert_eq!(listening["max_connections"], 8);
    assert_eq!(listening["idle_secs"], 30);
    assert_eq!(listening["denied_cidrs"], 1);

    // The probe.
    assert_eq!(
        proxy.ask(b"GET /healthz HTTP/1.1\r\nHost: probe\r\n\r\n"),
        (200, "ok\n".to_owned())
    );

    // A host that is not on the list.
    let (status, body) = proxy.ask(b"CONNECT evil.test:443 HTTP/1.1\r\nHost: evil.test:443\r\nProxy-Authorization: Basic c2VjcmV0\r\n\r\n");
    assert_eq!((status, body.as_str()), (403, "host not allowed\n"));
    let refusal = proxy.wait_for(|f| f["decision"] == "refuse" && f["host"] == "evil.test");
    assert_eq!(refusal["reason"], "host not allowed");
    assert_eq!(refusal["port"], 443);

    // A host that is on the list and resolves, through the system's resolver, to the loopback.
    let (status, body) = proxy.ask(b"CONNECT localhost:443 HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!((status, body.as_str()), (403, "no public address\n"));

    // A port that is not.
    let (status, _) = proxy.ask(b"CONNECT localhost:80 HTTP/1.1\r\n\r\n");
    assert_eq!(status, 403);

    // Plain HTTP, and an IP literal.
    let (status, body) = proxy.ask(b"GET http://extra.test/x?y=1 HTTP/1.1\r\n\r\n");
    assert_eq!(
        (status, body.as_str()),
        (403, "only HTTPS through CONNECT\n")
    );
    let (status, body) = proxy.ask(b"CONNECT 1.1.1.1:443 HTTP/1.1\r\n\r\n");
    assert_eq!((status, body.as_str()), (403, "ip literal\n"));

    proxy.sigterm();
    assert_eq!(proxy.exit_within(Duration::from_secs(10)), Some(0));
    let logs = proxy.seen.join("\n");
    assert!(
        !logs.contains("c2VjcmV0") && !logs.contains("Proxy-Authorization"),
        "{logs}"
    );
    assert!(!logs.contains("y=1"), "{logs}");
    // Everything it said was a structured line.
    proxy.wait_for(|fields| fields["message"] == "every connection has finished");
}
