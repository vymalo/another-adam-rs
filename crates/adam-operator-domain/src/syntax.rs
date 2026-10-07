//! Small syntax checks, each a rule of the adam-rs chart's `_validate.tpl` or of Kubernetes that a
//! CRD schema cannot say. Pure string functions.

/// A DNS-1123 label: lower-case letters, digits and `-`, starting and ending alphanumeric, at most
/// 63 characters.
pub fn is_dns_label(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b.first().is_some_and(|c| *c != b'-')
        && b.last().is_some_and(|c| *c != b'-')
}

/// A shell-safe environment variable name: `[A-Za-z_][A-Za-z0-9_]*`. Kubernetes allows more; an
/// adam `${VAR}` reference and a person reading a manifest are served by this.
pub fn is_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// An HTTP token, as a header name must be (RFC 9110 `token`): the chart's
/// `^[!#$%&'*+.^_`|~0-9A-Za-z-]+$`.
pub fn is_http_token(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+.^_`|~-".contains(c))
}

/// What `parse_http_url` found.
#[derive(Debug, PartialEq, Eq)]
pub struct HttpUrl<'a> {
    /// `http` or `https`.
    pub scheme: &'a str,
    /// Host and port, no user information.
    pub authority: &'a str,
    /// The host in lower case, without the port; an IPv6 host keeps its brackets.
    pub host: String,
}

/// Parse an `http` or `https` URL far enough to say what the operator needs: the scheme and the
/// host, and that there is no user name or password in it (a secret in a URL reaches the logs).
///
/// With `strict` the URL follows the chart's rule for an MCP server: after the authority only a
/// path or a query, and no `$` anywhere (no `${VAR}`).
pub fn parse_http_url(s: &str, strict: bool) -> Result<HttpUrl<'_>, &'static str> {
    if s.chars().any(char::is_whitespace) {
        return Err("it contains whitespace");
    }
    if strict && s.contains('$') {
        return Err("it contains `$` (no ${VAR} in a URL: it would reach the logs)");
    }
    let lower = s.to_ascii_lowercase();
    let (scheme, rest) = if lower.starts_with("https://") {
        ("https", &s["https://".len()..])
    } else if lower.starts_with("http://") {
        ("http", &s["http://".len()..])
    } else {
        return Err("it is not an http or https URL");
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() {
        return Err("it has no host");
    }
    if authority.contains('@') {
        return Err("it has a user name or password in it");
    }
    if strict && rest[end..].starts_with('#') {
        return Err("a fragment cannot follow the host");
    }
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        match stripped.find(']') {
            Some(i) => format!("[{}]", stripped[..i].to_ascii_lowercase()),
            None => return Err("its IPv6 host is not closed"),
        }
    } else {
        authority
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    if host.is_empty() {
        return Err("it has no host");
    }
    Ok(HttpUrl {
        scheme: &s[..scheme.len()],
        authority,
        host,
    })
}

/// Whether a URL is plain `http` to another machine, which adam refuses at startup unless
/// `MCP_ALLOW_INSECURE` is set: the rule of adam-rs `crates/adam-mcp/src/url.rs` as the chart
/// restates it (https, or http to `localhost`, `*.localhost`, `127.*` and `[::1]`, need nothing).
pub fn is_plain_http_remote(s: &str) -> bool {
    let Ok(url) = parse_http_url(s.trim(), false) else {
        return false;
    };
    if !url.scheme.eq_ignore_ascii_case("http") {
        return false;
    }
    let h = url.host.as_str();
    let loopback = h == "localhost"
        || h.ends_with(".localhost")
        || (h.starts_with("127.") && h.chars().all(|c| c.is_ascii_digit() || c == '.'))
        || h == "[::1]";
    !loopback
}

/// A Kubernetes quantity that is a size: [`is_quantity`] and a positive number (`500m`, `20Gi`,
/// `1e3`; not `0` or `0Gi`). Storage and volume sizes are sizes; resource requests and limits are
/// quantities, where `0` is legal.
pub fn is_size(s: &str) -> bool {
    is_quantity(s)
        && s.chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .any(|c| ('1'..='9').contains(&c))
}

/// A Kubernetes quantity: a non-negative number with an optional SI, binary or exponent suffix
/// (`0`, `500m`, `20Gi`, `1e3`).
pub fn is_quantity(s: &str) -> bool {
    let digits_end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (number, suffix) = s.split_at(digits_end);
    if number.is_empty()
        || number.matches('.').count() > 1
        || !number.chars().any(|c| c.is_ascii_digit())
    {
        return false;
    }
    const SUFFIXES: &[&str] = &[
        "", "Ki", "Mi", "Gi", "Ti", "Pi", "Ei", "n", "u", "m", "k", "M", "G", "T", "P", "E",
    ];
    if SUFFIXES.contains(&suffix) {
        return true;
    }
    // An exponent: e3, E-3, e+3.
    let Some(exp) = suffix.strip_prefix(['e', 'E']) else {
        return false;
    };
    let exp = exp.strip_prefix(['+', '-']).unwrap_or(exp);
    !exp.is_empty() && exp.chars().all(|c| c.is_ascii_digit())
}

/// A relative path inside a folder: not empty, not absolute, no empty, `.` or `..` segment, no
/// backslash and no NUL.
pub fn is_relative_file_path(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.contains(['\\', '\0'])
        && s.split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// An absolute container path with no empty, `.` or `..` segment, and not `/` itself.
pub fn is_mount_path(s: &str) -> bool {
    s.len() > 1
        && s.starts_with('/')
        && !s.contains(['\\', '\0'])
        && s[1..]
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// Whether two absolute paths are equal or one is inside the other.
pub fn paths_overlap(a: &str, b: &str) -> bool {
    let within = |outer: &str, inner: &str| {
        inner == outer
            || inner
                .strip_prefix(outer)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    within(a, b) || within(b, a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_labels() {
        for ok in ["coder", "a", "a-b-1", "x".repeat(63).as_str()] {
            assert!(is_dns_label(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", "A", "a_b", "a.b", "x".repeat(64).as_str()] {
            assert!(!is_dns_label(bad), "{bad}");
        }
    }

    #[test]
    fn env_names() {
        for ok in ["A", "_x", "SEARCH_MCP_TOKEN", "a1"] {
            assert!(is_env_name(ok), "{ok}");
        }
        for bad in ["", "1A", "A-B", "A.B", "A B", "private-key.pem"] {
            assert!(!is_env_name(bad), "{bad}");
        }
    }

    #[test]
    fn header_names() {
        assert!(is_http_token("Authorization"));
        assert!(is_http_token("X-Api-Key"));
        assert!(!is_http_token(""));
        assert!(!is_http_token("Bad Header"));
        assert!(!is_http_token("Bad:Header"));
    }

    #[test]
    fn urls() {
        let ok = parse_http_url("https://mcp.example.com/mcp", true).unwrap();
        assert_eq!((ok.scheme, ok.host.as_str()), ("https", "mcp.example.com"));
        assert_eq!(
            parse_http_url("http://h:8080/x?y=1", true)
                .unwrap()
                .authority,
            "h:8080"
        );
        assert_eq!(
            parse_http_url("HTTP://[::1]:80/", true).unwrap().host,
            "[::1]"
        );
        for (bad, why) in [
            ("ftp://x/", "not an http"),
            ("https://u:p@x/", "user name"),
            ("https:///x", "no host"),
            ("https://x/${TOKEN}", "$"),
            ("https://x#frag", "fragment"),
            ("https://x /y", "whitespace"),
            ("", "not an http"),
        ] {
            let err = parse_http_url(bad, true).unwrap_err();
            assert!(err.contains(why), "{bad}: {err}");
        }
        // Not strict: a fragment and a `$` are the caller's business.
        assert!(parse_http_url("https://x/a$b#f", false).is_ok());
    }

    #[test]
    fn plain_http_to_another_machine() {
        for remote in [
            "http://search.ns.svc:8080/mcp",
            "http://example.com",
            "http://10.0.0.1/mcp",
            "HTTP://Example.com/x",
        ] {
            assert!(is_plain_http_remote(remote), "{remote}");
        }
        for fine in [
            "https://example.com",
            "http://localhost:3000/mcp",
            "http://api.localhost/x",
            "http://127.0.0.1:8082",
            "http://[::1]:8080/",
            "not a url",
        ] {
            assert!(!is_plain_http_remote(fine), "{fine}");
        }
    }

    #[test]
    fn quantities() {
        for ok in [
            "500m", "1Gi", "20Gi", "5", "0.5", "1e3", "2E-3", "100M", "1.5Gi", "0", "0m",
        ] {
            assert!(is_quantity(ok), "{ok}");
        }
        for bad in ["", "Gi", "-1", "1.2.3", "1Xi", "1e", "5 Gi", "1gi", "abc"] {
            assert!(!is_quantity(bad), "{bad}");
        }
    }

    #[test]
    fn sizes() {
        for ok in ["500m", "1Gi", "20Gi", "5", "0.5", "1e3", "1.5Gi"] {
            assert!(is_size(ok), "{ok}");
        }
        for bad in ["", "0", "0Gi", "0.0", "-1", "1Xi", "abc"] {
            assert!(!is_size(bad), "{bad}");
        }
    }

    #[test]
    fn file_paths() {
        for ok in [
            "instructions.md",
            "skills/review/SKILL.md",
            ".hidden",
            "a/b",
        ] {
            assert!(is_relative_file_path(ok), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "a/",
            "./a",
            "a\\b",
        ] {
            assert!(!is_relative_file_path(bad), "{bad}");
        }
    }

    #[test]
    fn mounts() {
        assert!(is_mount_path("/work"));
        assert!(is_mount_path("/var/lib/x"));
        for bad in ["/", "work", "/a/", "/a/../b", "//a", ""] {
            assert!(!is_mount_path(bad), "{bad}");
        }
        assert!(paths_overlap("/etc/adam", "/etc/adam/agent"));
        assert!(paths_overlap("/etc/adam/agent", "/etc/adam"));
        assert!(paths_overlap("/work", "/work"));
        assert!(!paths_overlap("/etc/adam", "/etc/adamant"));
    }
}
