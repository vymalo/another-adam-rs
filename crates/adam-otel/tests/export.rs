//! The real exporter, not the in-memory one: a span reaches a collector over OTLP/HTTP.
//!
//! The collector is a plain TCP listener on loopback: enough to prove that `Otel::start` builds
//! the OTLP/HTTP exporter over the workspace's `reqwest` and that the batch task drives it on the
//! process's Tokio runtime.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt as _;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_span_reaches_a_collector_over_otlp_http() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    listener.set_nonblocking(true).expect("nonblocking");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .expect("timeout");
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                    let _ = tx.send(String::from_utf8_lossy(&head).into_owned());
                    return;
                }
                Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() > deadline {
                        let _ = tx.send(String::new());
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => {
                    let _ = tx.send(String::new());
                    return;
                }
            }
        }
    });

    let settings =
        adam_otel::Settings::enabled(format!("http://127.0.0.1:{port}")).expect("settings");
    let otel = adam_otel::Otel::start(&settings)
        .expect("start")
        .expect("an endpoint is set");
    let subscriber = Registry::default().with(otel.layer());
    tracing::subscriber::with_default(subscriber, || {
        tracing::info_span!("adam.run", state = "runnable").in_scope(|| {});
    });
    otel.shutdown();

    let head = rx.recv_timeout(Duration::from_secs(10)).unwrap_or_default();
    assert!(
        head.starts_with("POST /v1/traces "),
        "the collector got no OTLP/HTTP trace request: {head:?}"
    );
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/x-protobuf"),
        "the request is not OTLP/protobuf: {head:?}"
    );
}
