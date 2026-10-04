//! `mcp.json`. Field names follow Claude Code's `.mcp.json` and the draft SEP-2633 (there is no
//! ratified standard yet; *verified 2026-09-29*, <https://code.claude.com/docs/en/mcp.md>).

use std::collections::BTreeMap;
use std::path::Path;

use crate::diagnostic::{Diagnostic, Sink};
use crate::schema::{McpConfig, McpServer, RawMcp, RawServer, RemoteKind, scan_references};

/// The longest model-facing tool name (`<server>__<tool>`) providers accept, as the plan sets
/// it (the OpenAI function-name limit).
const MODEL_NAME_MAX: usize = 64;

/// Key names that would put a credential in the file, compared without `_`/`-` and lowercased.
const SECRET_KEYS: &[&str] = &["apikey", "token", "secret", "password", "accesstoken"];

/// Read `mcp.json`. `path` is the file, relative to the source root, for diagnostics.
///
/// `${VAR}` and `${VAR:-default}` stay in the values exactly as written. A server with an error
/// is left out; the rest are kept. A literal credential in a header, an environment variable or
/// a URL is a warning (a strict build refuses it); a key such as `apiKey` is an error.
pub fn parse_mcp(path: &Path, text: &str, diagnostics: &mut Vec<Diagnostic>) -> Option<McpConfig> {
    let mut sink = Sink {
        out: diagnostics,
        path,
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let raw: RawMcp = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(e) => {
            let line = u32::try_from(e.line()).ok().filter(|l| *l > 0);
            sink.error(line, format!("invalid JSON: {e}"));
            return None;
        }
    };
    for key in raw.extra.keys() {
        sink.warn(None, format!("unknown top-level key `{key}` is ignored"));
    }

    let mut servers = BTreeMap::new();
    for (name, server) in raw.servers {
        if let Some(s) = convert(&mut sink, &name, server) {
            servers.insert(name, s);
        }
    }
    Some(McpConfig { servers })
}

/// What `type` says.
#[derive(Clone, Copy)]
enum Declared {
    Unset,
    Stdio,
    Remote(RemoteKind),
}

fn convert(sink: &mut Sink<'_>, name: &str, raw: RawServer) -> Option<McpServer> {
    let mut ok = true;
    let mut fail = |sink: &mut Sink<'_>, message: String| {
        sink.error(None, message);
        ok = false;
    };

    if !server_name_ok(name) {
        fail(
            sink,
            format!(
                "server name `{name}` must be letters, digits, `-` and `_` (at most 64 characters, \
                 no `__`, not ending in `_`): the model sees its tools as `{name}__<tool>`"
            ),
        );
    }
    for key in raw.extra.keys() {
        let normalized: String = key
            .chars()
            .filter(|c| *c != '_' && *c != '-')
            .flat_map(char::to_lowercase)
            .collect();
        if SECRET_KEYS.contains(&normalized.as_str()) {
            fail(
                sink,
                format!(
                    "server `{name}`: `{key}` is not allowed: secrets belong in the environment; \
                     refer to them as `${{VAR}}` in `headers` or `env`"
                ),
            );
        } else {
            sink.warn(
                None,
                format!("server `{name}`: unknown key `{key}` is ignored"),
            );
        }
    }

    let declared = match raw.kind.as_deref().map(str::trim) {
        None => Declared::Unset,
        Some("stdio") => Declared::Stdio,
        Some("http") => Declared::Remote(RemoteKind::Http),
        Some("streamable-http") => Declared::Remote(RemoteKind::StreamableHttp),
        Some("sse") => Declared::Remote(RemoteKind::Sse),
        Some(other) => {
            fail(
                sink,
                format!(
                    "server `{name}`: `type: {other}` is not supported (use `stdio`, `http`, \
                     `streamable-http` or `sse`)"
                ),
            );
            return None;
        }
    };

    for entry in raw.tools.iter().flatten() {
        let fits = !entry.is_empty()
            && !entry.starts_with('_')
            && entry
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && name.len() + 2 + entry.len() <= MODEL_NAME_MAX;
        if !fits {
            fail(
                sink,
                format!(
                    "server `{name}`: tool `{entry}` cannot be named `{name}__{entry}` for the model \
                     (letters, digits, `-`, `_`, not starting with `_`; at most {MODEL_NAME_MAX} \
                     characters)"
                ),
            );
        }
    }
    for text in raw
        .args
        .iter()
        .chain(raw.env.values())
        .chain(raw.headers.values())
        .chain(raw.command.iter())
        .chain(raw.url.iter())
    {
        for bad in scan_references(text).malformed {
            sink.warn(
                None,
                format!("server `{name}`: `{bad}` is not a `${{VAR}}` or `${{VAR:-default}}` reference; it stays literal"),
            );
        }
    }

    let command = raw.command.as_deref().map(str::trim);
    let url = raw.url.as_deref().map(str::trim);
    let server = match (declared, command, url) {
        (Declared::Unset | Declared::Stdio, Some(""), None) => {
            fail(sink, format!("server `{name}` has an empty `command`"));
            return None;
        }
        (Declared::Unset | Declared::Stdio, Some(command), None) => {
            warn_literal_env(sink, name, &raw.env);
            McpServer::Stdio {
                command: command.to_owned(),
                args: raw.args,
                env: raw.env,
                tools: raw.tools,
                optional: raw.optional.unwrap_or(false),
            }
        }
        (Declared::Unset, None, Some(_)) => {
            fail(
                sink,
                format!("server `{name}` has a `url` but no `type`: add `type: http` (or `sse`)"),
            );
            return None;
        }
        (Declared::Unset, Some(_), Some(_)) => {
            fail(
                sink,
                format!(
                    "server `{name}` has both `command` and `url`: a server is one or the other"
                ),
            );
            return None;
        }
        (Declared::Unset, None, None) => {
            fail(
                sink,
                format!(
                    "server `{name}` needs a `command` (stdio), or a `type` and a `url` (remote)"
                ),
            );
            return None;
        }
        (Declared::Stdio, _, Some(_)) => {
            fail(sink, format!("server `{name}` is `stdio` but has a `url`"));
            return None;
        }
        (Declared::Stdio, None, None) => {
            fail(
                sink,
                format!("server `{name}` is `stdio` but has no `command`"),
            );
            return None;
        }
        (Declared::Remote(_), Some(_), _) => {
            fail(
                sink,
                format!("server `{name}` is remote (`type`) but has a `command`"),
            );
            return None;
        }
        (Declared::Remote(_), None, None) => {
            fail(sink, format!("server `{name}` is remote but has no `url`"));
            return None;
        }
        (Declared::Remote(_), None, Some(url)) if !url_ok(url) => {
            fail(
                sink,
                format!("server `{name}`: `url` must start with `http://` or `https://`"),
            );
            return None;
        }
        (Declared::Remote(kind), None, Some(url)) => {
            warn_literal_headers(sink, name, &raw.headers);
            warn_literal_url(sink, name, url);
            McpServer::Remote {
                kind,
                url: url.to_owned(),
                headers: raw.headers,
                tools: raw.tools,
                optional: raw.optional.unwrap_or(false),
            }
        }
    };
    ok.then_some(server)
}

/// A server name the model can be shown in `<server>__<tool>`. It has no `__` and does not end in
/// `_`: `a` with a tool `_x` and `a_` with a tool `x` would both be `a___x`, and the tools of `a_`
/// would look like tools of `a`.
fn server_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MODEL_NAME_MAX
        && !name.contains("__")
        && !name.ends_with('_')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `http(s)://...`, or a reference that expands to one (`${MCP_URL}`).
fn url_ok(url: &str) -> bool {
    let u = url.trim();
    u.starts_with("http://") || u.starts_with("https://") || u.starts_with("${")
}

fn secretish(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "authorization",
        "token",
        "secret",
        "password",
        "api-key",
        "api_key",
        "apikey",
        "credential",
    ]
    .iter()
    .any(|k| n.contains(k))
}

fn literal(value: &str) -> bool {
    let v = value.trim();
    !v.is_empty() && !v.contains("${")
}

fn warn_literal_headers(sink: &mut Sink<'_>, server: &str, headers: &BTreeMap<String, String>) {
    for (key, value) in headers {
        let scheme = value.trim().to_ascii_lowercase();
        let has_scheme = ["bearer ", "basic ", "token "]
            .iter()
            .any(|s| scheme.starts_with(s));
        if literal(value) && (has_scheme || secretish(key)) {
            sink.warn(
                None,
                format!(
                    "server `{server}`: header `{key}` holds what looks like a literal credential; \
                     write `${{VAR}}` and set the variable in the environment"
                ),
            );
        }
    }
}

fn warn_literal_env(sink: &mut Sink<'_>, server: &str, env: &BTreeMap<String, String>) {
    for (key, value) in env {
        if literal(value) && secretish(key) {
            sink.warn(
                None,
                format!(
                    "server `{server}`: env `{key}` holds what looks like a literal credential; \
                     write `${{VAR}}` and set the variable in the environment"
                ),
            );
        }
    }
}

fn warn_literal_url(sink: &mut Sink<'_>, server: &str, url: &str) {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    let userinfo_literal = authority
        .rsplit_once('@')
        .is_some_and(|(info, _)| literal(info));
    let query_literal = after_scheme
        .split_once('?')
        .map(|(_, q)| q.split('#').next().unwrap_or(q))
        .is_some_and(|q| {
            q.split('&').any(|pair| {
                pair.split_once('=')
                    .is_some_and(|(k, v)| secretish(k) && literal(v))
            })
        });
    if userinfo_literal || query_literal {
        sink.warn(
            None,
            format!(
                "server `{server}`: the `url` carries what looks like a literal credential; \
                 move it to a header (`Authorization: Bearer ${{VAR}}`) and set the variable in \
                 the environment (a `${{VAR}}` in a URL is refused at run time unless the \
                 deployment opts in)"
            ),
        );
    }
}
