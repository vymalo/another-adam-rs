//! The process's tracing subscriber: JSON `fmt()`, and the OTel layer beside it when
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set ([ADR 0075]).
//!
//! [ADR 0075]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0075-metrics-and-traces-are-off-by-default-and-say-nothing-about-a-person.md

use adam_otel::{Otel, OtelLayer, Settings};
use tracing::Subscriber;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer as _;
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Installs the subscriber and starts the exporter the environment names.
///
/// `None` when `OTEL_EXPORTER_OTLP_ENDPOINT` is unset: no exporter, no task, and the subscriber is
/// the `fmt()` layer alone. A setting the crate refuses, or an exporter it cannot build, is one
/// warning line and tracing stays off: a trace is never a reason a run fails (ADR 0075).
pub(crate) fn init(filter: EnvFilter) -> Option<Otel> {
    let (otel, problem) = start();
    compose(filter, otel.as_ref()).init();
    if let Some(problem) = problem {
        tracing::warn!(error = %problem, "tracing export is off");
    }
    otel
}

/// [`Settings::from_env`] then [`Otel::start`], with a refusal as a line rather than an exit.
fn start() -> (Option<Otel>, Option<String>) {
    let settings = match Settings::from_env() {
        Ok(settings) => settings,
        Err(error) => return (None, Some(error.to_string())),
    };
    match Otel::start(&settings) {
        Ok(otel) => (otel, None),
        Err(error) => (None, Some(error.to_string())),
    }
}

/// The subscriber: the `fmt()` layer the binary always wrote, and the OTel layer beside it.
///
/// `otel` is `None` when tracing is off, and an `Option<Layer>` composes as no layer at all.
fn compose(filter: EnvFilter, otel: Option<&Otel>) -> impl Subscriber + Send + Sync + 'static {
    Registry::default()
        .with(tracing_subscriber::fmt::layer().json().with_filter(filter))
        .with(otel_layer(otel))
}

/// The OTel half of [`compose`]: `None` when tracing is off, so no layer is built.
fn otel_layer<S>(otel: Option<&Otel>) -> Option<OtelLayer<S>>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    otel.map(|otel| otel.layer())
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;

    /// Off is off: with no endpoint the binary builds no exporter and composes no OTel layer.
    #[test]
    fn without_an_endpoint_nothing_is_built_and_no_layer_is_composed() {
        // A blank endpoint is unset, and a name the crate does not read changes nothing.
        let settings = Settings::from_pairs([
            (adam_otel::ENDPOINT_VAR, "  "),
            ("OTEL_SOMETHING_ELSE", "http://collector:4318"),
        ])
        .expect("settings");
        assert!(!settings.is_enabled());
        // Outside a Tokio runtime: an endpoint would answer `NoRuntime` here, so `Ok(None)` is
        // proof that nothing was built.
        let otel = Otel::start(&settings).expect("off is not an error");
        assert!(otel.is_none(), "an exporter was built without an endpoint");
        assert!(
            otel_layer::<Registry>(otel.as_ref()).is_none(),
            "an OTel layer was composed without an endpoint"
        );
    }

    /// The other half: this binary's composition posts one run span to a collector over OTLP/HTTP.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_run_span_reaches_a_loopback_collector() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let (send, received) = mpsc::channel();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .expect("timeout");
                        let request = read_request(&mut stream);
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                        let _ = send.send(request);
                        return;
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() > deadline {
                            let _ = send.send(Vec::new());
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => {
                        let _ = send.send(Vec::new());
                        return;
                    }
                }
            }
        });

        let settings = Settings::enabled(format!("http://127.0.0.1:{port}")).expect("settings");
        let otel = Otel::start(&settings)
            .expect("start")
            .expect("an endpoint is set");
        let subscriber = compose(EnvFilter::new("info"), Some(&otel));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info_span!("adam.run", state = "runnable").in_scope(|| {});
        });
        // The batch task exports on its own timer: a loaded machine gets 30 s, so this tests the
        // composition and not the scheduler.
        let request = received
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_default();
        otel.shutdown();
        let text = String::from_utf8_lossy(&request);
        assert!(
            request.windows(15).any(|w| w == b"POST /v1/traces"),
            "the collector got no OTLP/HTTP trace request: {text:?}"
        );
        assert!(
            request.windows(8).any(|w| w == b"adam.run"),
            "the exported request carries no run span: {text:?}"
        );
    }

    /// The whole request: the head and the body its `Content-Length` announces.
    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return request,
                Ok(n) => request.extend_from_slice(&buf[..n]),
            }
        }
        let head_end = request
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|at| at + 4)
            .unwrap_or(request.len());
        let head = String::from_utf8_lossy(&request[..head_end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while request.len() < head_end + length {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => request.extend_from_slice(&buf[..n]),
            }
        }
        request
    }
}
