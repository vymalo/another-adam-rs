//! Why a request is refused, and what the client is told.

use std::fmt;

use adam_error::{Classify, ErrorClass};

/// A request the proxy does not carry out, each with the status the client gets and the reason the
/// log line gives. The client is told the reason in a plain-text body and nothing else: no host
/// list, no address, no detail about the network behind the proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// The head is not an HTTP request head.
    Malformed,
    /// The head is longer than the limit, or has too many headers.
    HeadTooLarge,
    /// The head did not arrive within the limit.
    HeadTimeout,
    /// A request that is not `CONNECT`: plain HTTP, which run pods have no use for.
    PlainHttp,
    /// `CONNECT` to an IP address, IPv4 or `[IPv6]`, instead of a name.
    IpLiteral,
    /// `CONNECT` to a port outside `EGRESS_PORTS`.
    PortNotAllowed,
    /// `CONNECT` to a name that is not on the allow-list.
    HostNotAllowed,
    /// The name resolved, and every address it gave is private, special or IPv6.
    NoPublicAddress,
    /// The name did not resolve.
    ResolutionFailed,
    /// The name did not resolve in time.
    ResolutionTimeout,
    /// No address of the name accepted the connection.
    ConnectFailed,
    /// The addresses of the name did not answer in time.
    ConnectTimeout,
    /// The proxy is serving as many connections as it may.
    AtCapacity,
}

impl Refusal {
    /// The HTTP status of the answer.
    pub const fn status(self) -> u16 {
        match self {
            Self::Malformed => 400,
            Self::HeadTimeout => 408,
            Self::HeadTooLarge => 431,
            Self::PlainHttp
            | Self::IpLiteral
            | Self::PortNotAllowed
            | Self::HostNotAllowed
            | Self::NoPublicAddress => 403,
            Self::ResolutionFailed | Self::ConnectFailed => 502,
            Self::AtCapacity => 503,
            Self::ResolutionTimeout | Self::ConnectTimeout => 504,
        }
    }

    /// The reason, as the log line has it.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Malformed => "malformed request",
            Self::HeadTooLarge => "request head too large",
            Self::HeadTimeout => "request head timed out",
            Self::PlainHttp => "plain http request",
            Self::IpLiteral => "ip literal",
            Self::PortNotAllowed => "port not allowed",
            Self::HostNotAllowed => "host not allowed",
            Self::NoPublicAddress => "no public address",
            Self::ResolutionFailed => "name does not resolve",
            Self::ResolutionTimeout => "name resolution timed out",
            Self::ConnectFailed => "no address connected",
            Self::ConnectTimeout => "connect timed out",
            Self::AtCapacity => "too many connections",
        }
    }

    /// What the client reads in the body of the answer.
    pub const fn body(self) -> &'static str {
        match self {
            Self::PlainHttp => "only HTTPS through CONNECT",
            other => other.reason(),
        }
    }

    /// The whole answer: the status line, three headers, a blank line and the body. The connection
    /// is closed after it.
    pub fn response(self) -> Vec<u8> {
        response(self.status(), self.body())
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl std::error::Error for Refusal {}

impl Classify for Refusal {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Malformed
            | Self::HeadTooLarge
            | Self::PlainHttp
            | Self::IpLiteral
            | Self::PortNotAllowed
            | Self::HostNotAllowed
            | Self::NoPublicAddress => ErrorClass::Invalid,
            Self::HeadTimeout
            | Self::ResolutionFailed
            | Self::ResolutionTimeout
            | Self::ConnectFailed
            | Self::ConnectTimeout
            | Self::AtCapacity => ErrorClass::Transient,
        }
    }
}

/// An HTTP/1.1 answer with a plain-text body (a newline is added to it), closing the connection.
pub(crate) fn response(status: u16, body: &str) -> Vec<u8> {
    let phrase = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        408 => "Request Timeout",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    format!(
        "HTTP/1.1 {status} {phrase}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}\n",
        body.len() + 1
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Refusal; 13] = [
        Refusal::Malformed,
        Refusal::HeadTooLarge,
        Refusal::HeadTimeout,
        Refusal::PlainHttp,
        Refusal::IpLiteral,
        Refusal::PortNotAllowed,
        Refusal::HostNotAllowed,
        Refusal::NoPublicAddress,
        Refusal::ResolutionFailed,
        Refusal::ResolutionTimeout,
        Refusal::ConnectFailed,
        Refusal::ConnectTimeout,
        Refusal::AtCapacity,
    ];

    /// Exhaustive: a new variant forces a decision about its status and its class here.
    #[test]
    fn every_refusal_has_its_status_and_its_class() {
        use ErrorClass::{Invalid, Transient};
        for refusal in ALL {
            let (status, class) = match refusal {
                Refusal::Malformed => (400, Invalid),
                Refusal::HeadTimeout => (408, Transient),
                Refusal::HeadTooLarge => (431, Invalid),
                Refusal::PlainHttp => (403, Invalid),
                Refusal::IpLiteral => (403, Invalid),
                Refusal::PortNotAllowed => (403, Invalid),
                Refusal::HostNotAllowed => (403, Invalid),
                Refusal::NoPublicAddress => (403, Invalid),
                Refusal::ResolutionFailed => (502, Transient),
                Refusal::ConnectFailed => (502, Transient),
                Refusal::AtCapacity => (503, Transient),
                Refusal::ResolutionTimeout => (504, Transient),
                Refusal::ConnectTimeout => (504, Transient),
            };
            assert_eq!(refusal.status(), status, "{refusal}");
            assert_eq!(refusal.class(), class, "{refusal}");
        }
    }

    #[test]
    fn reasons_are_distinct_and_the_body_of_plain_http_says_what_to_do() {
        let reasons: std::collections::BTreeSet<_> = ALL.iter().map(|r| r.reason()).collect();
        assert_eq!(reasons.len(), ALL.len());
        assert_eq!(Refusal::PlainHttp.body(), "only HTTPS through CONNECT");
        assert_eq!(Refusal::NoPublicAddress.body(), "no public address");
        assert_eq!(Refusal::HostNotAllowed.to_string(), "host not allowed");
    }

    #[test]
    fn the_answer_is_a_complete_http_response_that_closes() {
        let text = String::from_utf8(Refusal::PlainHttp.response()).unwrap();
        assert_eq!(
            text,
            "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain; charset=utf-8\r\n\
             Content-Length: 27\r\nConnection: close\r\n\r\nonly HTTPS through CONNECT\n"
        );
    }
}
