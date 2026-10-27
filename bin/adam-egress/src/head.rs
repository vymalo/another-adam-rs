//! Reading and understanding the request head.
//!
//! Only the request line decides anything: the headers are parsed (so that a head with a broken
//! one is refused) and then ignored, in particular `Proxy-Authorization`, which is neither read
//! nor kept nor logged. Nothing of the head except a host and a port leaves this module.

use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::time::timeout;

use crate::allow::{HostName, NameError};
use crate::config::Limits;
use crate::refusal::Refusal;

/// The most headers a head may have; one more is `431`.
const MAX_HEADERS: usize = 64;
/// The bytes asked of the socket at a time.
const CHUNK: usize = 2048;

/// What the client asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    /// `CONNECT host:port`: the name is valid and lowercased; allowed or not is decided later.
    Connect { host: HostName, port: u16 },
    /// `GET /healthz`, the probe of the pod. A proxy request is never in this form.
    Health,
}

/// A request that is refused before anything is resolved, with what may be logged of it: only a
/// host that is a valid name (or a short address) and a port, never a path or a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused {
    pub refusal: Refusal,
    pub host: Option<String>,
    pub port: Option<u16>,
    /// What the proxy found out while deciding (a range, an error kind), for the log only.
    pub detail: Option<String>,
}

impl Refused {
    pub(crate) fn new(refusal: Refusal, host: Option<String>, port: Option<u16>) -> Self {
        Self {
            refusal,
            host,
            port,
            detail: None,
        }
    }

    pub(crate) fn detail(mut self, detail: impl Into<Option<String>>) -> Self {
        self.detail = detail.into();
        self
    }
}

impl From<Refusal> for Refused {
    fn from(refusal: Refusal) -> Self {
        Self::new(refusal, None, None)
    }
}

/// A head that was read: the request, and the bytes that came after it in the same reads (a client
/// that sends its first TLS bytes before it has the `200`).
#[derive(Debug)]
pub(crate) struct Head {
    pub request: Request,
    pub leftover: Vec<u8>,
}

/// Why no head came out of the socket.
#[derive(Debug)]
pub(crate) enum HeadFailure {
    /// The client went away, or the socket failed: nobody to answer.
    Closed,
    /// The client is answered with this.
    Refused(Refused),
}

/// Read the head from `stream`: at most `limits.max_head` bytes, all within `limits.head_timeout`.
pub(crate) async fn read_head<R: AsyncRead + Unpin>(
    stream: &mut R,
    limits: &Limits,
) -> Result<Head, HeadFailure> {
    match timeout(limits.head_timeout, read(stream, limits.max_head)).await {
        Ok(head) => head,
        Err(_) => Err(HeadFailure::Refused(Refusal::HeadTimeout.into())),
    }
}

async fn read<R: AsyncRead + Unpin>(stream: &mut R, max: usize) -> Result<Head, HeadFailure> {
    let mut buf = Vec::with_capacity(CHUNK.min(max));
    loop {
        match parse(&buf) {
            Ok(Some((len, request))) => {
                let leftover = buf.split_off(len);
                return Ok(Head { request, leftover });
            }
            Ok(None) => {}
            Err(refused) => return Err(HeadFailure::Refused(refused)),
        }
        // Never read past the limit: a head that is not complete in `max` bytes is too large.
        let room = max.saturating_sub(buf.len());
        if room == 0 {
            return Err(HeadFailure::Refused(Refusal::HeadTooLarge.into()));
        }
        let mut chunk = [0_u8; CHUNK];
        match stream.read(&mut chunk[..room.min(CHUNK)]).await {
            Ok(0) | Err(_) => return Err(HeadFailure::Closed),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Parse `buf` as a request head: `Ok(None)` while it is incomplete, else the length of the head
/// and what it asks for.
pub(crate) fn parse(buf: &[u8]) -> Result<Option<(usize, Request)>, Refused> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut head = httparse::Request::new(&mut headers);
    let len = match head.parse(buf) {
        Ok(httparse::Status::Complete(len)) => len,
        Ok(httparse::Status::Partial) => return Ok(None),
        Err(httparse::Error::TooManyHeaders) => return Err(Refusal::HeadTooLarge.into()),
        Err(_) => return Err(Refusal::Malformed.into()),
    };
    let method = head.method.unwrap_or_default();
    let target = head.path.unwrap_or_default();
    let request = match method {
        "CONNECT" => {
            let (host, port) = authority(target)?;
            Request::Connect { host, port }
        }
        "GET" if target == "/healthz" => Request::Health,
        _ => return Err(plain_http(target)),
    };
    Ok(Some((len, request)))
}

/// `host:port`, the target of a `CONNECT`.
fn authority(target: &str) -> Result<(HostName, u16), Refused> {
    let malformed = Refused::from(Refusal::Malformed);
    let (host, port) = target.rsplit_once(':').ok_or_else(|| malformed.clone())?;
    // Digits only: `parse` alone would take a leading `+`.
    let port = Some(port)
        .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|p| p.parse::<u16>().ok())
        .filter(|p| *p != 0)
        .ok_or(malformed)?;
    // `[::1]`, and an IPv6 address without its brackets, which has more colons.
    if host.starts_with('[') || host.contains(':') {
        return Err(literal(host, port));
    }
    match HostName::parse(host) {
        Ok(name) => Ok((name, port)),
        // `10.0.0.1`, `127.1`, `2130706433`: addresses that a resolver would read as such.
        Err(NameError::Numeric) => Err(literal(host, port)),
        Err(_) => Err(Refused::new(Refusal::Malformed, None, Some(port))),
    }
}

fn literal(host: &str, port: u16) -> Refused {
    Refused::new(Refusal::IpLiteral, loggable_literal(host), Some(port))
}

/// An address as the client wrote it, when it is short and made of the characters of addresses;
/// anything else is not worth a log line.
fn loggable_literal(host: &str) -> Option<String> {
    (host.len() <= 64
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'.' | b'[' | b']' | b'%')))
    .then(|| host.to_owned())
}

/// A request that is not a `CONNECT`: plain HTTP. If its target is an absolute URL, the host and
/// the port of it are what the log may say (not the user information, the path or the query).
fn plain_http(target: &str) -> Refused {
    let (host, port) = absolute_host(target).unwrap_or((None, None));
    Refused::new(Refusal::PlainHttp, host, port)
}

fn absolute_host(target: &str) -> Option<(Option<String>, Option<u16>)> {
    let (scheme, rest) = target.split_once("://")?;
    if scheme.is_empty() || !scheme.bytes().all(|b| b.is_ascii_alphabetic()) {
        // `/path?next=http://x`: an origin-form target with a URL in its query.
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => (host, port.parse().ok()),
        _ => (authority, None),
    };
    Some((HostName::parse(host).ok().map(|h| h.to_string()), port))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn connect(target: &str) -> Result<Request, Refused> {
        let head = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
        parse(head.as_bytes()).map(|parsed| parsed.expect("complete").1)
    }

    fn refusal_of(head: &str) -> Refused {
        parse(head.as_bytes()).unwrap_err()
    }

    #[test]
    fn a_connect_to_a_name_and_a_port_is_understood() {
        assert_eq!(
            connect("Registry.NPMJS.org.:443"),
            Ok(Request::Connect {
                host: HostName::parse("registry.npmjs.org").unwrap(),
                port: 443
            })
        );
        assert_eq!(
            connect("a.example:8443"),
            Ok(Request::Connect {
                host: HostName::parse("a.example").unwrap(),
                port: 8443
            })
        );
        // HTTP/1.0 is a head too.
        let (len, request) = parse(b"CONNECT a.example:443 HTTP/1.0\r\n\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(len, 34);
        assert!(matches!(request, Request::Connect { .. }));
    }

    #[test]
    fn the_head_ends_at_the_blank_line_and_the_rest_is_not_its() {
        let bytes = b"CONNECT a.example:443 HTTP/1.1\r\nHost: a\r\n\r\n\x16\x03\x01hello";
        let (len, _) = parse(bytes).unwrap().unwrap();
        assert_eq!(&bytes[len..], b"\x16\x03\x01hello");
    }

    #[test]
    fn a_head_that_is_not_finished_is_not_a_decision() {
        assert_eq!(parse(b""), Ok(None));
        assert_eq!(
            parse(b"CONNECT a.example:443 HTTP/1.1\r\nHost: a"),
            Ok(None)
        );
        assert_eq!(parse(b"CONNECT a.example:443 HTTP/1.1\r\n"), Ok(None));
    }

    #[test]
    fn an_ip_literal_is_refused_as_one_whatever_its_shape() {
        for target in [
            "10.0.0.1:443",
            "1.1.1.1:443",
            "127.0.0.1:443",
            "127.1:443",
            "2130706433:443",
            "1.2.3:443",
            "999.999.999.999:443",
            "[::1]:443",
            "[2606:4700:4700::1111]:443",
            "::1:443",
            "2001:db8::1:443",
            "[::ffff:10.0.0.1]:443",
        ] {
            let refused = connect(target).unwrap_err();
            assert_eq!(refused.refusal, Refusal::IpLiteral, "{target}");
            assert_eq!(refused.port, Some(443), "{target}");
        }
        let refused = connect("10.0.0.1:443").unwrap_err();
        assert_eq!(refused.host.as_deref(), Some("10.0.0.1"));
    }

    #[test]
    fn a_target_that_is_not_host_and_port_is_malformed() {
        for target in [
            "example.com",
            "example.com:",
            ":443",
            "example.com:0",
            "example.com:65536",
            "example.com:-1",
            "example.com:+443",
            "example.com:44 3",
            "example.com:https",
            "/index.html",
            "user@example.com:443",
            "exa_mple.com:443",
            "ex%61mple.com:443",
            "example..com:443",
            "-example.com:443",
        ] {
            let refused = connect(target).unwrap_err();
            assert_eq!(refused.refusal, Refusal::Malformed, "{target}");
            assert_eq!(
                refused.host, None,
                "a malformed host is never logged: {target}"
            );
        }
    }

    #[test]
    fn a_head_that_is_not_http_is_malformed() {
        for head in [
            "\x16\x03\x01\x02\x00\x01\x00\x01\u{fc}\x03\x03\r\n\r\n",
            "CONNECT\r\n\r\n",
            "CONNECT a.example:443\r\n\r\n",
            "CONNECT a.example:443 HTTP/9.9\r\n\r\n",
            "CONNECT a.example:443 FTP/1.1\r\n\r\n",
            "CONNECT a.example:443 HTTP/1.1\r\nBad Header: x\r\n\r\n",
            "CONNECT a.example:443 HTTP/1.1\r\nno-colon\r\n\r\n",
            "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n",
        ] {
            assert_eq!(refusal_of(head).refusal, Refusal::Malformed, "{head:?}");
        }
    }

    #[test]
    fn too_many_headers_is_a_head_too_large() {
        let mut head = String::from("CONNECT a.example:443 HTTP/1.1\r\n");
        for i in 0..=MAX_HEADERS {
            head.push_str(&format!("X-{i}: v\r\n"));
        }
        head.push_str("\r\n");
        assert_eq!(refusal_of(&head).refusal, Refusal::HeadTooLarge);
    }

    #[test]
    fn healthz_is_the_only_request_that_is_not_a_connect_and_is_answered() {
        assert_eq!(
            parse(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap()
                .unwrap()
                .1,
            Request::Health
        );
        for head in [
            "GET /healthz?x=1 HTTP/1.1\r\n\r\n",
            "GET /healthz/ HTTP/1.1\r\n\r\n",
            "HEAD /healthz HTTP/1.1\r\n\r\n",
            "POST /healthz HTTP/1.1\r\n\r\n",
            "GET /healthz2 HTTP/1.1\r\n\r\n",
            "GET http://a.example/healthz HTTP/1.1\r\n\r\n",
        ] {
            assert_eq!(refusal_of(head).refusal, Refusal::PlainHttp, "{head:?}");
        }
    }

    #[test]
    fn plain_http_is_refused_and_the_log_gets_a_host_and_no_more() {
        let refused = refusal_of(
            "GET http://user:hunter2@Example.com:8080/private/path?token=abc#frag HTTP/1.1\r\n\
             Proxy-Authorization: Basic c2VjcmV0\r\n\r\n",
        );
        assert_eq!(refused.refusal, Refusal::PlainHttp);
        assert_eq!(refused.host.as_deref(), Some("example.com"));
        assert_eq!(refused.port, Some(8080));
        let shown = format!("{refused:?}");
        for secret in ["hunter2", "private", "token", "abc", "frag", "c2VjcmV0"] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
    }

    #[test]
    fn what_is_not_an_absolute_url_gives_no_host() {
        for target in [
            "/",
            "/some/path?next=http://evil.example/",
            "*",
            "example.com:80",
            "http://",
            "http://[::1]/x",
            "http://bad_host/x",
            "://x/y",
        ] {
            let refused = refusal_of(&format!("GET {target} HTTP/1.1\r\n\r\n"));
            assert_eq!(refused.refusal, Refusal::PlainHttp, "{target}");
            assert_eq!(refused.host, None, "{target}");
        }
        for method in ["POST", "PUT", "DELETE", "OPTIONS", "HEAD", "get", "connect"] {
            let refused = refusal_of(&format!("{method} http://a.example/ HTTP/1.1\r\n\r\n"));
            assert_eq!(refused.refusal, Refusal::PlainHttp, "{method}");
            assert_eq!(refused.host.as_deref(), Some("a.example"));
            assert_eq!(refused.port, None);
        }
    }

    fn limits(max_head: usize, head_timeout_ms: u64) -> Limits {
        Limits {
            max_head,
            head_timeout: Duration::from_millis(head_timeout_ms),
            ..Limits::default()
        }
    }

    #[tokio::test]
    async fn a_head_is_read_from_a_stream_with_what_follows_it() {
        let mut bytes: &[u8] = b"CONNECT a.example:443 HTTP/1.1\r\n\r\nGET / ...";
        let head = read_head(&mut bytes, &limits(8192, 1000)).await.unwrap();
        assert!(matches!(head.request, Request::Connect { port: 443, .. }));
        assert_eq!(head.leftover, b"GET / ...");
    }

    #[tokio::test]
    async fn a_head_that_does_not_fit_is_too_large_and_is_not_read_past_the_limit() {
        let mut head = b"CONNECT a.example:443 HTTP/1.1\r\n".to_vec();
        head.extend(std::iter::repeat_n(b'x', 10_000));
        let mut stream: &[u8] = &head;
        let Err(HeadFailure::Refused(refused)) = read_head(&mut stream, &limits(256, 1000)).await
        else {
            panic!("expected a refusal");
        };
        assert_eq!(refused.refusal, Refusal::HeadTooLarge);
        assert_eq!(stream.len(), head.len() - 256, "reads stopped at the limit");

        // A head of exactly the limit is fine.
        let line = b"CONNECT a.example:443 HTTP/1.1\r\n\r\n";
        let mut stream: &[u8] = line;
        assert!(
            read_head(&mut stream, &limits(line.len(), 1000))
                .await
                .is_ok()
        );
        let mut stream: &[u8] = line;
        let Err(HeadFailure::Refused(refused)) =
            read_head(&mut stream, &limits(line.len() - 1, 1000)).await
        else {
            panic!("one byte short is too large");
        };
        assert_eq!(refused.refusal, Refusal::HeadTooLarge);
    }

    #[tokio::test]
    async fn a_head_that_is_too_slow_is_timed_out_however_it_trickles() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let trickle = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            for byte in b"CONNECT a.example:443 HTTP/1.1\r\n" {
                if client.write_all(&[*byte]).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let Err(HeadFailure::Refused(refused)) = read_head(&mut server, &limits(8192, 150)).await
        else {
            panic!("expected a timeout");
        };
        assert_eq!(refused.refusal, Refusal::HeadTimeout);
        trickle.abort();
    }

    #[tokio::test]
    async fn a_client_that_leaves_is_nobody_to_answer() {
        let mut empty: &[u8] = b"";
        assert!(matches!(
            read_head(&mut empty, &limits(8192, 1000)).await,
            Err(HeadFailure::Closed)
        ));
        let mut half: &[u8] = b"CONNECT a.example:443 HT";
        assert!(matches!(
            read_head(&mut half, &limits(8192, 1000)).await,
            Err(HeadFailure::Closed)
        ));
    }
}
