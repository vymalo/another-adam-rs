//! The proxy on a loopback listener: an injected resolver, a local upstream, and the log captured.
//! No network. Every test runs on the current-thread runtime of `#[tokio::test]`, so the log
//! subscriber installed for the test sees every task the proxy spawns.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_egress::{AddressFilter, AllowList, Cidr, Config, HostName, Limits, Resolver, Server};
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tracing_subscriber::fmt::MakeWriter;

const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const ESTABLISHED: &str = "HTTP/1.1 200 Connection Established\r\n\r\n";

// ---------------------------------------------------------------------------------------- logs

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Logs {
    type Writer = Logs;

    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

impl Logs {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    /// The `fields` of every log line that has a `decision`.
    fn decisions(&self) -> Vec<Value> {
        self.text()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .map(|line| line["fields"].clone())
            .filter(|fields| fields.get("decision").is_some())
            .collect()
    }
}

/// Capture what this test logs, down to debug, as JSON lines like the binary's.
fn capture() -> (Logs, tracing::subscriber::DefaultGuard) {
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    (logs, tracing::subscriber::set_default(subscriber))
}

// ------------------------------------------------------------------------------------ fixtures

/// A resolver with a table, and optionally a delay before it answers.
#[derive(Default)]
struct Names {
    table: HashMap<&'static str, Vec<IpAddr>>,
    delay: Option<Duration>,
}

impl Names {
    fn with(mut self, name: &'static str, addresses: &[IpAddr]) -> Self {
        self.table.insert(name, addresses.to_vec());
        self
    }
}

#[async_trait]
impl Resolver for Names {
    async fn resolve(&self, host: &HostName) -> io::Result<Vec<IpAddr>> {
        if let Some(delay) = self.delay {
            sleep(delay).await;
        }
        self.table
            .get(host.as_str())
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such name"))
    }
}

fn allow(entries: &[&str]) -> AllowList {
    let mut list = AllowList::new();
    for entry in entries {
        list.add(entry).unwrap();
    }
    list
}

/// A configuration for a test: short limits, the given ports and allow-list.
fn config(ports: &[u16], entries: &[&str]) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        allow: allow(entries),
        ports: ports.iter().copied().collect(),
        deny_cidrs: Vec::new(),
        idle: Duration::from_secs(10),
        max_connections: 32,
        limits: Limits {
            max_head: 1024,
            head_timeout: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(2),
            max_attempts: 4,
            shutdown_grace: Duration::from_secs(2),
        },
    }
}

/// A running proxy.
struct Proxy {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Proxy {
    async fn start(config: Config, resolver: Names, filter: AddressFilter) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = Server::new(config, Arc::new(resolver), filter);
        let task = tokio::spawn(async move {
            server
                .serve(listener, async {
                    let _ = stopped.await;
                })
                .await;
        });
        Self {
            addr,
            stop: Some(stop),
            task,
        }
    }

    /// Stop it and wait for it to finish; how long that took.
    async fn shut_down(mut self) -> Duration {
        let began = Instant::now();
        let _ = self.stop.take().unwrap().send(());
        timeout(Duration::from_secs(10), &mut self.task)
            .await
            .expect("the server ends")
            .unwrap();
        began.elapsed()
    }

    async fn connect(&self) -> TcpStream {
        TcpStream::connect(self.addr).await.unwrap()
    }
}

/// A server that echoes everything back and counts its connections.
struct Echo {
    port: u16,
    accepted: Arc<Mutex<usize>>,
    task: JoinHandle<()>,
}

impl Echo {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(Mutex::new(0));
        let count = accepted.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                *count.lock().unwrap() += 1;
                tokio::spawn(async move {
                    let (mut read, mut write) = stream.split();
                    let _ = tokio::io::copy(&mut read, &mut write).await;
                    let _ = write.shutdown().await;
                });
            }
        });
        Self {
            port,
            accepted,
            task,
        }
    }

    fn accepted(&self) -> usize {
        *self.accepted.lock().unwrap()
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A port nothing listens on.
async fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// What the proxy answered to a request that it did not tunnel.
#[derive(Debug)]
struct Answer {
    status: u16,
    head: String,
    body: String,
}

/// Send `bytes`, read until the proxy closes, and read the answer.
async fn ask(addr: SocketAddr, bytes: &[u8]) -> Answer {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(bytes).await.unwrap();
    answer(&mut stream).await
}

async fn answer(stream: &mut TcpStream) -> Answer {
    let mut raw = Vec::new();
    timeout(Duration::from_secs(5), stream.read_to_end(&mut raw))
        .await
        .expect("the proxy closes after its answer")
        .unwrap();
    let text = String::from_utf8(raw).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").expect("a complete head");
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    Answer {
        status,
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

fn connect_head(target: &str) -> Vec<u8> {
    format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").into_bytes()
}

/// Read the proxy's answer to a `CONNECT` that was tunnelled, one byte at a time so that none of
/// the tunnel's bytes is taken.
async fn read_established(stream: &mut TcpStream) {
    let mut seen = Vec::new();
    while !seen.ends_with(b"\r\n\r\n") {
        let byte = timeout(Duration::from_secs(5), stream.read_u8())
            .await
            .expect("an answer")
            .unwrap();
        seen.push(byte);
    }
    assert_eq!(String::from_utf8(seen).unwrap(), ESTABLISHED);
}

async fn read_exactly(stream: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut data = vec![0_u8; n];
    timeout(Duration::from_secs(5), stream.read_exact(&mut data))
        .await
        .expect("the bytes arrive")
        .unwrap();
    data
}

/// The filter that treats the loopback as public, which the tests need to reach their upstream.
fn open_filter() -> AddressFilter {
    AddressFilter::only(Vec::new())
}

// ------------------------------------------------------------------------------------- tunnels

#[tokio::test]
async fn an_allowed_host_is_tunnelled_and_the_bytes_sent_with_the_head_arrive() {
    let (logs, _guard) = capture();
    let echo = Echo::start().await;
    let proxy = Proxy::start(
        config(&[echo.port], &["allowed.test"]),
        Names::default().with("allowed.test", &[LOCALHOST]),
        open_filter(),
    )
    .await;

    let mut stream = proxy.connect().await;
    // The head and the first bytes of the tunnel in one write, as a client that does not wait for
    // the 200 sends them.
    let mut first = connect_head(&format!("allowed.test:{}", echo.port));
    first.extend_from_slice(b"pipelined");
    stream.write_all(&first).await.unwrap();
    read_established(&mut stream).await;
    assert_eq!(read_exactly(&mut stream, 9).await, b"pipelined");

    // And both ways after that, in several pieces.
    for piece in [&b"one"[..], b"two two", &[0_u8, 255, 10, 13][..]] {
        stream.write_all(piece).await.unwrap();
        assert_eq!(read_exactly(&mut stream, piece.len()).await, piece);
    }
    let big: Vec<u8> = (0..200_000_u32).map(|i| (i % 251) as u8).collect();
    let (mut read, mut write) = stream.split();
    let (sent, echoed) = tokio::join!(
        async {
            write.write_all(&big).await.unwrap();
        },
        async {
            let mut back = vec![0_u8; big.len()];
            timeout(Duration::from_secs(10), read.read_exact(&mut back))
                .await
                .expect("the echo arrives")
                .unwrap();
            back
        }
    );
    let () = sent;
    assert_eq!(echoed, big);

    let decisions = logs.decisions();
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert_eq!(decisions[0]["decision"], "allow");
    assert_eq!(decisions[0]["host"], "allowed.test");
    assert_eq!(decisions[0]["port"], echo.port);
    assert_eq!(decisions[0]["upstream"], "127.0.0.1");
    assert!(
        decisions[0]["client"]
            .as_str()
            .unwrap()
            .starts_with("127.0.0.1:")
    );
    assert_eq!(echo.accepted(), 1);
}

#[tokio::test]
async fn a_closed_client_side_still_gets_the_rest_of_the_answer() {
    let (_logs, _guard) = capture();
    let echo = Echo::start().await;
    let proxy = Proxy::start(
        config(&[echo.port], &["allowed.test"]),
        Names::default().with("allowed.test", &[LOCALHOST]),
        open_filter(),
    )
    .await;
    let mut stream = proxy.connect().await;
    stream
        .write_all(&connect_head(&format!("allowed.test:{}", echo.port)))
        .await
        .unwrap();
    read_established(&mut stream).await;
    stream.write_all(b"last words").await.unwrap();
    stream.shutdown().await.unwrap();
    let mut rest = Vec::new();
    timeout(Duration::from_secs(5), stream.read_to_end(&mut rest))
        .await
        .expect("both directions close")
        .unwrap();
    assert_eq!(rest, b"last words");
}

#[tokio::test]
async fn a_subdomain_is_tunnelled_by_a_suffix_entry_and_the_apex_is_not() {
    let (logs, _guard) = capture();
    let echo = Echo::start().await;
    let proxy = Proxy::start(
        config(&[echo.port], &[".files.test"]),
        Names::default()
            .with("a.b.files.test", &[LOCALHOST])
            .with("files.test", &[LOCALHOST]),
        open_filter(),
    )
    .await;
    let mut stream = proxy.connect().await;
    stream
        .write_all(&connect_head(&format!("A.B.files.test.:{}", echo.port)))
        .await
        .unwrap();
    read_established(&mut stream).await;

    let refused = ask(
        proxy.addr,
        &connect_head(&format!("files.test:{}", echo.port)),
    )
    .await;
    assert_eq!(refused.status, 403);
    assert_eq!(refused.body, "host not allowed\n");
    assert_eq!(
        echo.accepted(),
        1,
        "only the subdomain reached the upstream"
    );
    let decisions = logs.decisions();
    assert_eq!(decisions[0]["host"], "a.b.files.test", "{decisions:?}");
}

// -------------------------------------------------------------------------------------- refusals

#[tokio::test]
async fn a_host_that_is_not_allowed_is_403_and_nothing_is_resolved_or_connected() {
    let (logs, _guard) = capture();
    let echo = Echo::start().await;
    let proxy = Proxy::start(
        config(&[echo.port], &["allowed.test"]),
        // The name resolves, to the upstream: only the allow-list stops it.
        Names::default().with("other.test", &[LOCALHOST]),
        open_filter(),
    )
    .await;
    let refused = ask(
        proxy.addr,
        &connect_head(&format!("other.test:{}", echo.port)),
    )
    .await;
    assert_eq!(refused.status, 403, "{refused:?}");
    assert_eq!(refused.body, "host not allowed\n");
    assert!(refused.head.contains("Connection: close"), "{refused:?}");
    assert!(
        refused.head.contains("Content-Type: text/plain"),
        "{refused:?}"
    );
    assert_eq!(echo.accepted(), 0);
    let decisions = logs.decisions();
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert_eq!(decisions[0]["decision"], "refuse");
    assert_eq!(decisions[0]["reason"], "host not allowed");
    assert_eq!(decisions[0]["host"], "other.test");
    assert_eq!(decisions[0]["status"], 403);
}

#[tokio::test]
async fn a_port_that_is_not_allowed_is_403() {
    let (logs, _guard) = capture();
    let echo = Echo::start().await;
    let proxy = Proxy::start(
        config(&[echo.port], &["allowed.test"]),
        Names::default().with("allowed.test", &[LOCALHOST]),
        open_filter(),
    )
    .await;
    for port in [22, 80, 8443, 65535] {
        let refused = ask(proxy.addr, &connect_head(&format!("allowed.test:{port}"))).await;
        assert_eq!(
            (refused.status, refused.body.as_str()),
            (403, "port not allowed\n"),
            "{port}"
        );
    }
    assert_eq!(echo.accepted(), 0);
    assert_eq!(logs.decisions()[0]["reason"], "port not allowed");
    assert_eq!(logs.decisions()[0]["port"], 22);
}

#[tokio::test]
async fn an_ip_literal_is_403_even_when_it_is_the_upstream() {
    let (logs, _guard) = capture();
    let echo = Echo::start().await;
    let proxy = Proxy::start(
        // The allow-list holds the literal's text too: an entry that is no name is no entry.
        config(&[echo.port], &["allowed.test"]),
        Names::default(),
        open_filter(),
    )
    .await;
    for target in [
        format!("127.0.0.1:{}", echo.port),
        format!("[::1]:{}", echo.port),
        format!("[::ffff:127.0.0.1]:{}", echo.port),
        format!("2130706433:{}", echo.port),
        format!("0x7f.0.0.1:{}", echo.port),
    ] {
        let refused = ask(proxy.addr, &connect_head(&target)).await;
        assert_eq!(
            (refused.status, refused.body.as_str()),
            (403, "ip literal\n"),
            "{target}"
        );
    }
    assert_eq!(echo.accepted(), 0);
    let decisions = logs.decisions();
    assert!(
        decisions.iter().all(|d| d["reason"] == "ip literal"),
        "{decisions:?}"
    );
    assert_eq!(decisions[0]["host"], "127.0.0.1");
}

#[tokio::test]
async fn a_name_that_resolves_to_a_private_address_is_403_no_public_address() {
    let (logs, _guard) = capture();
    let echo = Echo::start().await;
    // The standard filter: the loopback is not public, and a server really listens there.
    let proxy = Proxy::start(
        config(
            &[echo.port],
            &["rebind.test", "mixed.test", "six.test", "empty.test"],
        ),
        Names::default()
            .with("rebind.test", &[LOCALHOST])
            .with(
                "mixed.test",
                &[
                    "10.1.2.3".parse().unwrap(),
                    "169.254.169.254".parse().unwrap(),
                    "::1".parse().unwrap(),
                    "::ffff:10.0.0.1".parse().unwrap(),
                    "::ffff:8.8.8.8".parse().unwrap(),
                ],
            )
            .with("six.test", &["2606:4700:4700::1111".parse().unwrap()])
            .with("empty.test", &[]),
        AddressFilter::standard(Vec::new()),
    )
    .await;
    for name in ["rebind.test", "mixed.test", "six.test", "empty.test"] {
        let refused = ask(proxy.addr, &connect_head(&format!("{name}:{}", echo.port))).await;
        assert_eq!(
            (refused.status, refused.body.as_str()),
            (403, "no public address\n"),
            "{name}"
        );
        assert!(!refused.body.contains("127.0.0.1") && !refused.body.contains("10."));
    }
    assert_eq!(echo.accepted(), 0, "no connection was attempted");
    let decisions = logs.decisions();
    assert_eq!(decisions[0]["reason"], "no public address");
    assert!(
        decisions[0]["detail"]
            .as_str()
            .unwrap()
            .contains("loopback"),
        "{decisions:?}"
    );
    assert!(
        decisions[1]["detail"].as_str().unwrap().contains("private"),
        "{decisions:?}"
    );
    assert!(
        decisions[2]["detail"].as_str().unwrap().contains("IPv6"),
        "{decisions:?}"
    );
    assert!(
        decisions[3]["detail"]
            .as_str()
            .unwrap()
            .contains("no address"),
        "{decisions:?}"
    );
}

#[tokio::test]
async fn the_private_addresses_of_a_name_are_skipped_and_the_next_one_is_tried() {
    let (logs, _guard) = capture();
    let echo = Echo::start().await;
    let proxy = Proxy::start(
        config(&[echo.port], &["multi.test"]),
        Names::default().with(
            "multi.test",
            &[
                "10.0.0.9".parse().unwrap(),
                "::1".parse().unwrap(),
                LOCALHOST,
            ],
        ),
        // Only the 10/8 range is dropped here, so the loopback is the address that is kept.
        AddressFilter::only(vec![Cidr::parse("10.0.0.0/8").unwrap().unwrap()]),
    )
    .await;
    let mut stream = proxy.connect().await;
    stream
        .write_all(&connect_head(&format!("multi.test:{}", echo.port)))
        .await
        .unwrap();
    read_established(&mut stream).await;
    assert_eq!(logs.decisions()[0]["upstream"], "127.0.0.1");
}

#[tokio::test]
async fn the_deployments_denied_ranges_apply() {
    let (_logs, _guard) = capture();
    let echo = Echo::start().await;
    let proxy = Proxy::start(
        config(&[echo.port], &["cluster.test"]),
        Names::default().with("cluster.test", &["10.96.0.10".parse().unwrap()]),
        AddressFilter::standard(vec![Cidr::parse("10.96.0.0/12").unwrap().unwrap()]),
    )
    .await;
    let refused = ask(
        proxy.addr,
        &connect_head(&format!("cluster.test:{}", echo.port)),
    )
    .await;
    assert_eq!(refused.status, 403);
}

#[tokio::test]
async fn plain_http_is_403_only_https_through_connect() {
    let (_logs, _guard) = capture();
    let echo = Echo::start().await;
    let proxy = Proxy::start(
        config(&[echo.port], &["allowed.test"]),
        Names::default().with("allowed.test", &[LOCALHOST]),
        open_filter(),
    )
    .await;
    for request in [
        "GET http://allowed.test/ HTTP/1.1\r\nHost: allowed.test\r\n\r\n",
        "POST http://allowed.test/upload HTTP/1.1\r\nHost: allowed.test\r\nContent-Length: 0\r\n\r\n",
        "GET / HTTP/1.1\r\nHost: allowed.test\r\n\r\n",
        "OPTIONS * HTTP/1.1\r\n\r\n",
    ] {
        let refused = ask(proxy.addr, request.as_bytes()).await;
        assert_eq!(refused.status, 403, "{request:?}");
        assert_eq!(refused.body, "only HTTPS through CONNECT\n", "{request:?}");
    }
    assert_eq!(echo.accepted(), 0);
}

#[tokio::test]
async fn healthz_answers_200_ok_and_is_not_logged() {
    let (logs, _guard) = capture();
    let proxy = Proxy::start(
        config(&[443], &["allowed.test"]),
        Names::default(),
        AddressFilter::standard(Vec::new()),
    )
    .await;
    let health = ask(proxy.addr, b"GET /healthz HTTP/1.1\r\nHost: probe\r\n\r\n").await;
    assert_eq!((health.status, health.body.as_str()), (200, "ok\n"));
    assert!(logs.decisions().is_empty(), "{}", logs.text());
}

#[tokio::test]
async fn a_head_that_is_not_http_is_400_too_large_is_431_and_too_slow_is_408() {
    let (_logs, _guard) = capture();
    let mut config = config(&[443], &["allowed.test"]);
    config.limits.head_timeout = Duration::from_millis(1000);
    let proxy = Proxy::start(
        config,
        Names::default(),
        AddressFilter::standard(Vec::new()),
    )
    .await;

    let tls = ask(
        proxy.addr,
        b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03\r\n\r\n",
    )
    .await;
    assert_eq!(
        (tls.status, tls.body.as_str()),
        (400, "malformed request\n")
    );
    let no_port = ask(proxy.addr, &connect_head("allowed.test")).await;
    assert_eq!(no_port.status, 400);

    // A head of 20 KiB where 1 KiB is the limit: the answer arrives although the client is still
    // writing, and the connection is not reset under it.
    let mut stream = proxy.connect().await;
    let mut huge = b"CONNECT allowed.test:443 HTTP/1.1\r\nX-Padding: ".to_vec();
    huge.extend(std::iter::repeat_n(b'x', 20_000));
    huge.extend_from_slice(b"\r\n\r\n");
    stream.write_all(&huge).await.unwrap();
    let too_large = answer(&mut stream).await;
    assert_eq!(
        (too_large.status, too_large.body.as_str()),
        (431, "request head too large\n")
    );

    // Half a head, and then nothing.
    let mut stream = proxy.connect().await;
    stream.write_all(b"CONNECT allowed.test:44").await.unwrap();
    let began = Instant::now();
    let too_slow = answer(&mut stream).await;
    assert_eq!(
        (too_slow.status, too_slow.body.as_str()),
        (408, "request head timed out\n")
    );
    assert!(
        began.elapsed() >= Duration::from_millis(900),
        "{:?}",
        began.elapsed()
    );

    // A client that leaves in the middle of its head is nobody's answer, and costs nothing.
    let mut stream = proxy.connect().await;
    stream.write_all(b"CONNECT allowed.t").await.unwrap();
    drop(stream);
    let health = ask(proxy.addr, b"GET /healthz HTTP/1.1\r\n\r\n").await;
    assert_eq!(health.status, 200);
}

#[tokio::test]
async fn a_name_that_does_not_resolve_is_502_and_one_that_is_too_slow_is_504() {
    let (logs, _guard) = capture();
    let mut slow = config(&[443], &["gone.test", "slow.test"]);
    slow.limits.connect_timeout = Duration::from_millis(200);
    let proxy = Proxy::start(
        slow,
        Names {
            delay: None,
            ..Names::default()
        },
        open_filter(),
    )
    .await;
    let gone = ask(proxy.addr, &connect_head("gone.test:443")).await;
    assert_eq!(
        (gone.status, gone.body.as_str()),
        (502, "name does not resolve\n")
    );
    assert_eq!(logs.decisions()[0]["detail"], "no such name");
    proxy.shut_down().await;

    let mut slow = config(&[443], &["slow.test"]);
    slow.limits.connect_timeout = Duration::from_millis(200);
    let proxy = Proxy::start(
        slow,
        Names {
            delay: Some(Duration::from_secs(30)),
            ..Names::default().with("slow.test", &[LOCALHOST])
        },
        open_filter(),
    )
    .await;
    let began = Instant::now();
    let timed_out = ask(proxy.addr, &connect_head("slow.test:443")).await;
    assert_eq!(
        (timed_out.status, timed_out.body.as_str()),
        (504, "name resolution timed out\n")
    );
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
}

#[tokio::test]
async fn an_address_that_refuses_the_connection_is_502() {
    let (logs, _guard) = capture();
    let port = closed_port().await;
    let proxy = Proxy::start(
        config(&[port], &["down.test"]),
        Names::default().with("down.test", &[LOCALHOST]),
        open_filter(),
    )
    .await;
    let refused = ask(proxy.addr, &connect_head(&format!("down.test:{port}"))).await;
    assert_eq!(
        (refused.status, refused.body.as_str()),
        (502, "no address connected\n")
    );
    let decisions = logs.decisions();
    assert_eq!(decisions[0]["reason"], "no address connected");
    assert_eq!(decisions[0]["detail"], "connection refused");
}

#[tokio::test]
async fn an_address_that_does_not_answer_is_504() {
    let (_logs, _guard) = capture();
    // A listener that never accepts, with a backlog of one: once its queue is full the kernel drops
    // further SYNs and a connect waits.
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let blocked = socket.listen(1).unwrap();
    let port = blocked.local_addr().unwrap().port();
    let mut fillers = Vec::new();
    for _ in 0..8 {
        if let Ok(Ok(stream)) = timeout(
            Duration::from_millis(100),
            TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        {
            fillers.push(stream);
        }
    }
    assert!(
        fillers.len() < 8,
        "the listen queue never filled, so a connect would not wait"
    );
    let mut quick = config(&[port], &["stuck.test"]);
    quick.limits.connect_timeout = Duration::from_millis(300);
    let proxy = Proxy::start(
        quick,
        Names::default().with("stuck.test", &[LOCALHOST]),
        open_filter(),
    )
    .await;
    let began = Instant::now();
    let timed_out = ask(proxy.addr, &connect_head(&format!("stuck.test:{port}"))).await;
    assert_eq!(
        (timed_out.status, timed_out.body.as_str()),
        (504, "connect timed out\n")
    );
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    drop(fillers);
    drop(blocked);
}

// ------------------------------------------------------------------------------ limits and shutdown

#[tokio::test]
async fn above_the_connection_cap_the_answer_is_503_and_a_slot_given_back_is_used() {
    let (_logs, _guard) = capture();
    let mut small = config(&[443], &["allowed.test"]);
    small.max_connections = 2;
    let proxy = Proxy::start(small, Names::default(), AddressFilter::standard(Vec::new())).await;

    // Two connections that say nothing hold the two slots.
    let holders = [proxy.connect().await, proxy.connect().await];
    sleep(Duration::from_millis(50)).await;
    let refused = ask(proxy.addr, b"GET /healthz HTTP/1.1\r\n\r\n").await;
    assert_eq!(
        (refused.status, refused.body.as_str()),
        (503, "too many connections\n")
    );

    // The proxy notices the closes, and the slots come back.
    drop(holders);
    let began = Instant::now();
    loop {
        let health = ask(proxy.addr, b"GET /healthz HTTP/1.1\r\n\r\n").await;
        if health.status == 200 {
            break;
        }
        assert_eq!(health.status, 503);
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "the slots never came back"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn a_tunnel_that_moves_no_byte_is_closed_after_the_idle_limit() {
    let (_logs, _guard) = capture();
    let echo = Echo::start().await;
    let mut quick = config(&[echo.port], &["allowed.test"]);
    quick.idle = Duration::from_millis(1000);
    let proxy = Proxy::start(
        quick,
        Names::default().with("allowed.test", &[LOCALHOST]),
        open_filter(),
    )
    .await;

    // Traffic keeps it open for longer than the limit...
    let mut stream = proxy.connect().await;
    stream
        .write_all(&connect_head(&format!("allowed.test:{}", echo.port)))
        .await
        .unwrap();
    read_established(&mut stream).await;
    for _ in 0..8 {
        sleep(Duration::from_millis(150)).await;
        stream.write_all(b"x").await.unwrap();
        assert_eq!(read_exactly(&mut stream, 1).await, b"x");
    }
    // ...and silence closes it.
    let began = Instant::now();
    let mut rest = Vec::new();
    timeout(Duration::from_secs(5), stream.read_to_end(&mut rest))
        .await
        .expect("closed by the proxy")
        .unwrap();
    assert!(rest.is_empty());
    let silent = began.elapsed();
    assert!(
        (Duration::from_millis(800)..Duration::from_secs(5)).contains(&silent),
        "{silent:?}"
    );
}

#[tokio::test]
async fn shutdown_stops_accepting_waits_for_a_tunnel_and_gives_up_after_the_grace() {
    let (_logs, _guard) = capture();
    let echo = Echo::start().await;
    let mut quick = config(&[echo.port], &["allowed.test"]);
    quick.limits.shutdown_grace = Duration::from_secs(3);
    let proxy = Proxy::start(
        quick,
        Names::default().with("allowed.test", &[LOCALHOST]),
        open_filter(),
    )
    .await;
    let addr = proxy.addr;

    let mut open = proxy.connect().await;
    open.write_all(&connect_head(&format!("allowed.test:{}", echo.port)))
        .await
        .unwrap();
    read_established(&mut open).await;

    // The tunnel is still open when it is told to stop; it ends the wait by closing in time.
    let closer = tokio::spawn(async move {
        sleep(Duration::from_millis(200)).await;
        drop(open);
    });
    let took = proxy.shut_down().await;
    closer.await.unwrap();
    assert!(
        took >= Duration::from_millis(150),
        "waited for the tunnel: {took:?}"
    );
    assert!(
        took < Duration::from_millis(2500),
        "and not for the whole grace: {took:?}"
    );
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "the listener is closed once the server has ended"
    );
}

#[tokio::test]
async fn a_tunnel_that_outlasts_the_grace_does_not_hold_the_shutdown_for_ever() {
    let (_logs, _guard) = capture();
    let echo = Echo::start().await;
    let mut quick = config(&[echo.port], &["allowed.test"]);
    quick.limits.shutdown_grace = Duration::from_millis(300);
    let proxy = Proxy::start(
        quick,
        Names::default().with("allowed.test", &[LOCALHOST]),
        open_filter(),
    )
    .await;
    let mut open = proxy.connect().await;
    open.write_all(&connect_head(&format!("allowed.test:{}", echo.port)))
        .await
        .unwrap();
    read_established(&mut open).await;
    let took = proxy.shut_down().await;
    assert!(
        (Duration::from_millis(250)..Duration::from_secs(2)).contains(&took),
        "{took:?}"
    );
    drop(open);
}

// ------------------------------------------------------------------------------------------ logs

#[tokio::test]
async fn a_refusal_logs_no_header_no_path_and_no_query() {
    let (logs, _guard) = capture();
    let proxy = Proxy::start(
        config(&[443], &["allowed.test"]),
        Names::default(),
        AddressFilter::standard(Vec::new()),
    )
    .await;
    let request = "GET http://user:hunter2@example.net:8080/private/path?token=s3cr3t-token&x=1#frag \
                   HTTP/1.1\r\nHost: example.net\r\nProxy-Authorization: Basic c2VjcmV0LWNyZWRz\r\n\
                   Authorization: Bearer sk-live-123\r\nCookie: session=abc\r\n\r\n";
    let refused = ask(proxy.addr, request.as_bytes()).await;
    assert_eq!(refused.status, 403);

    // A CONNECT that is refused carries them too.
    let connect = "CONNECT evil.example:443 HTTP/1.1\r\nHost: evil.example:443\r\n\
                   Proxy-Authorization: Basic c2VjcmV0LWNyZWRz\r\nX-Token: s3cr3t-token\r\n\r\n";
    assert_eq!(ask(proxy.addr, connect.as_bytes()).await.status, 403);

    let text = logs.text();
    for secret in [
        "hunter2",
        "private",
        "path",
        "s3cr3t",
        "token",
        "frag",
        "c2VjcmV0",
        "Proxy-Authorization",
        "proxy-authorization",
        "Authorization",
        "Bearer",
        "sk-live",
        "Cookie",
        "session=abc",
        "x=1",
        "X-Token",
        "Basic",
    ] {
        assert!(!text.contains(secret), "{secret:?} is in the log:\n{text}");
    }
    let decisions = logs.decisions();
    assert_eq!(decisions.len(), 2, "{text}");
    assert_eq!(decisions[0]["decision"], "refuse");
    assert_eq!(decisions[0]["reason"], "plain http request");
    assert_eq!(
        decisions[0]["host"], "example.net",
        "the host of the URL is the one thing said"
    );
    assert_eq!(decisions[0]["port"], 8080);
    assert_eq!(decisions[1]["host"], "evil.example");
    assert_eq!(decisions[1]["port"], 443);
}

#[tokio::test]
async fn an_allowed_tunnel_logs_no_header_either() {
    let (logs, _guard) = capture();
    let echo = Echo::start().await;
    let proxy = Proxy::start(
        config(&[echo.port], &["allowed.test"]),
        Names::default().with("allowed.test", &[LOCALHOST]),
        open_filter(),
    )
    .await;
    let mut stream = proxy.connect().await;
    stream
        .write_all(
            format!(
                "CONNECT allowed.test:{0} HTTP/1.1\r\nHost: allowed.test:{0}\r\n\
                 Proxy-Authorization: Basic c2VjcmV0LWNyZWRz\r\nUser-Agent: npm/10 secret-agent\r\n\r\n",
                echo.port
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    read_established(&mut stream).await;
    stream.write_all(b"hello").await.unwrap();
    assert_eq!(read_exactly(&mut stream, 5).await, b"hello");
    drop(stream);
    sleep(Duration::from_millis(100)).await;
    let text = logs.text();
    for secret in [
        "c2VjcmV0",
        "Proxy-Authorization",
        "secret-agent",
        "npm/10",
        "hello",
    ] {
        assert!(!text.contains(secret), "{secret:?} is in the log:\n{text}");
    }
    assert_eq!(logs.decisions().len(), 1, "{text}");
}
