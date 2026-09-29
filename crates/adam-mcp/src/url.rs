//! The rules for the URL of a remote server, the same as for a remote subagent (`a2a:`): https,
//! or http only to this machine unless the deployment allows it; never credentials in the URL.
//!
//! `is_local` and `shown` are copied from `adam-assembly/src/remote.rs` (slice S9b), where they
//! guard the URL of a remote subagent; the two crates do not depend on each other, and the rule
//! is small enough to keep in step by hand (each has a test for it).

use url::{Host, Url};

use crate::error::{Error, UrlProblem};
use crate::redact::Redactor;

/// Whether `url` points at this machine: `localhost`, a `*.localhost` name, or a loopback address.
fn is_local(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(name)) => {
            let name = name.to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost")
        }
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

/// `url` without a user name, a password, a query or a fragment: what can be shown and logged.
pub(crate) fn shown(url: &Url) -> String {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

/// Check the URL of `server` (already expanded) against the rules. The URL an error shows has been
/// through `redactor`: without the opt-in [`McpPolicy::allow_url_secrets`] the URL holds no value
/// of a variable, and with it a value in the path or the host must not reach a message.
///
/// [`McpPolicy::allow_url_secrets`]: crate::McpPolicy::allow_url_secrets
pub(crate) fn check(
    server: &str,
    text: &str,
    allow_insecure: bool,
    redactor: &Redactor,
) -> Result<Url, Error> {
    let refuse = |url: String, problem| Error::Url {
        server: server.to_owned(),
        url,
        problem,
    };
    let Ok(url) = Url::parse(text.trim()) else {
        return Err(refuse("<not a URL>".to_owned(), UrlProblem::Unparseable));
    };
    let printable = redactor.scrub(&shown(&url));
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err(refuse(printable, UrlProblem::NotHttp));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(refuse(printable, UrlProblem::Credentials));
    }
    if url.scheme() == "http" && !is_local(&url) && !allow_insecure {
        return Err(refuse(printable, UrlProblem::Insecure));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problem(url: &str, allow_insecure: bool) -> Option<UrlProblem> {
        match check("linear", url, allow_insecure, &Redactor::default()) {
            Err(Error::Url { problem, .. }) => Some(problem),
            Ok(_) => None,
            Err(other) => panic!("{other}"),
        }
    }

    #[test]
    fn tls_required_unless_local_or_allowed() {
        for ok in [
            "https://mcp.example.com/mcp",
            "http://localhost:8080/mcp",
            "http://LOCALHOST/mcp",
            "http://tools.localhost/mcp",
            "http://127.0.0.1:9/mcp",
            "http://127.9.9.9/mcp",
            "http://[::1]:9/mcp",
        ] {
            assert_eq!(problem(ok, false), None, "{ok}");
        }
        for bad in [
            "http://mcp.example.com/mcp",
            "http://10.0.0.5/mcp",
            "http://localhost.example.com/mcp",
            "http://notlocalhost/mcp",
            "http://[2001:db8::1]/mcp",
        ] {
            assert_eq!(problem(bad, false), Some(UrlProblem::Insecure), "{bad}");
            assert_eq!(problem(bad, true), None, "{bad} with the opt-in");
        }
        assert_eq!(
            problem("ftp://mcp.example.com/", true),
            Some(UrlProblem::NotHttp)
        );
        assert_eq!(problem("not a url", true), Some(UrlProblem::Unparseable));
    }

    #[test]
    fn url_credentials_refused_and_never_shown() {
        let error = check(
            "linear",
            "https://carol-admin:hunter2@mcp.example.com/mcp?key=sekrit#frag",
            false,
            &Redactor::default(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            Error::Url {
                problem: UrlProblem::Credentials,
                ..
            }
        ));
        let text = error.to_string();
        for leaked in ["carol-admin", "hunter2", "sekrit", "frag", "key="] {
            assert!(!text.contains(leaked), "{leaked} in {text}");
        }
        assert!(text.contains("https://mcp.example.com/mcp"), "{text}");

        // A query is not refused (some servers want one), and never shown.
        let ok = check(
            "linear",
            "https://mcp.example.com/mcp?key=sekrit",
            false,
            &Redactor::default(),
        )
        .unwrap();
        assert_eq!(shown(&ok), "https://mcp.example.com/mcp");
        let insecure = check(
            "linear",
            "http://mcp.example.com/mcp?key=sekrit",
            false,
            &Redactor::default(),
        )
        .unwrap_err();
        assert!(!insecure.to_string().contains("sekrit"), "{insecure}");
    }

    #[test]
    fn a_registered_value_in_the_path_or_host_is_not_shown() {
        // With the opt-in a variable may be anywhere in the URL, so what an error prints of it goes
        // through the redactor: a plain value, and one the URL printer has percent-encoded.
        let mut redactor = Redactor::default();
        redactor.add("tok-91ac");
        redactor.add("sp ace/ü");
        for text in [
            "http://mcp.example.com/tok-91ac/mcp",
            "ftp://tok-91ac.example.com/mcp",
            "http://mcp.example.com/sp ace/ü/mcp",
        ] {
            let error = check("linear", text, false, &redactor).unwrap_err();
            let shown = error.to_string();
            assert!(shown.contains("[REDACTED]"), "{shown}");
            for leaked in ["tok-91ac", "sp%20ace", "sp ace", "%C3%BC", "ü"] {
                assert!(!shown.contains(leaked), "{leaked} in {shown}");
            }
        }
    }
}
