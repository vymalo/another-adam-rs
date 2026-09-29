//! `mcp.json`: the `mcpServers` shape of Claude Code's `.mcp.json` and the draft SEP-2633.
//!
//! `${VAR}` and `${VAR:-default}` are **never** expanded here: expansion happens at run time,
//! from the process environment, so that a secret never passes through the build. This module
//! only finds the references ([`McpConfig::env_references`]) so a build can record their names.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::names::is_env_name;

/// The transport of a remote MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RemoteKind {
    /// `type: http`.
    Http,
    /// `type: streamable-http`.
    StreamableHttp,
    /// `type: sse`.
    Sse,
}

/// One MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum McpServer {
    /// A process the runtime spawns (`command`; `type` is `stdio` or absent).
    Stdio {
        /// The executable.
        command: String,
        /// Its arguments.
        args: Vec<String>,
        /// Its environment; values are unexpanded text.
        env: BTreeMap<String, String>,
        /// The allow-list of tools (an adam extension); `None` allows all.
        tools: Option<Vec<String>>,
    },
    /// A server reached over the network (`type` and `url`).
    Remote {
        /// The transport.
        kind: RemoteKind,
        /// The endpoint; unexpanded text.
        url: String,
        /// Request headers; values are unexpanded text.
        headers: BTreeMap<String, String>,
        /// The allow-list of tools (an adam extension); `None` allows all.
        tools: Option<Vec<String>>,
    },
}

impl McpServer {
    /// The allow-list of tools, when there is one.
    pub fn tools(&self) -> Option<&[String]> {
        match self {
            Self::Stdio { tools, .. } | Self::Remote { tools, .. } => tools.as_deref(),
        }
    }

    fn texts(&self) -> Vec<&str> {
        match self {
            Self::Stdio {
                command, args, env, ..
            } => std::iter::once(command.as_str())
                .chain(args.iter().map(String::as_str))
                .chain(env.values().map(String::as_str))
                .collect(),
            Self::Remote { url, headers, .. } => std::iter::once(url.as_str())
                .chain(headers.values().map(String::as_str))
                .collect(),
        }
    }
}

/// A parsed `mcp.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct McpConfig {
    /// The servers by name. The model sees their tools as `<server>__<tool>`.
    pub servers: BTreeMap<String, McpServer>,
}

impl McpConfig {
    /// The names of every environment variable the config refers to with `${NAME}` or
    /// `${NAME:-default}`, sorted. Names only, never values.
    pub fn env_references(&self) -> BTreeSet<String> {
        self.servers
            .values()
            .flat_map(McpServer::texts)
            .flat_map(|t| scan_references(t).refs)
            .map(|r| r.name)
            .collect()
    }
}

/// One `${NAME}` or `${NAME:-default}` in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvRef {
    /// The variable.
    pub name: String,
    /// The text after `:-`, when there is one.
    pub default: Option<String>,
}

impl EnvRef {
    /// The reference as it is written: `${NAME}` or `${NAME:-default}`.
    pub fn written(&self) -> String {
        match &self.default {
            Some(default) => format!("${{{}:-{default}}}", self.name),
            None => format!("${{{}}}", self.name),
        }
    }
}

/// A piece of a text, as [`split_env_references`] cuts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment<'a> {
    /// Text that stays as it is, including any `${` that does not form a reference.
    Literal(&'a str),
    /// A `${NAME}` or `${NAME:-default}` to be replaced.
    Ref(EnvRef),
}

/// Cut `text` into literal text and `${NAME}` / `${NAME:-default}` references, in order. The
/// grammar of `mcp.json` values, in one place: the build finds the names to record with it
/// ([`McpConfig::env_references`]) and the run-time expansion (`adam-mcp`) replaces exactly the
/// same spans.
///
/// A reference runs from `${` to the first `}`; the name is what precedes the first `:-`, and
/// must be an environment variable name ([`is_env_name`]). A `${` with no `}`, or with anything
/// else as a name, is not a reference: it stays inside the literal text as written. Adjacent
/// literal text is one segment, and no segment is empty.
pub fn split_env_references(text: &str) -> Vec<Segment<'_>> {
    let mut segments = Vec::new();
    let mut literal_from = 0;
    let mut at = 0;
    while let Some(found) = text[at..].find("${") {
        let start = at + found;
        let inner_from = start + 2;
        let Some(end) = text[inner_from..].find('}') else {
            break;
        };
        let inner = &text[inner_from..inner_from + end];
        let stop = inner_from + end + 1;
        let (name, default) = match inner.split_once(":-") {
            Some((name, default)) => (name, Some(default.to_owned())),
            None => (inner, None),
        };
        if is_env_name(name) {
            if literal_from < start {
                segments.push(Segment::Literal(&text[literal_from..start]));
            }
            segments.push(Segment::Ref(EnvRef {
                name: name.to_owned(),
                default,
            }));
            literal_from = stop;
        }
        at = stop;
    }
    if literal_from < text.len() {
        segments.push(Segment::Literal(&text[literal_from..]));
    }
    segments
}

/// The references found in a text, and the `${` that do not form one.
#[derive(Debug, Default)]
pub(crate) struct Scan {
    pub(crate) refs: Vec<EnvRef>,
    /// The malformed spans, as written.
    pub(crate) malformed: Vec<String>,
}

/// Find `${NAME}` and `${NAME:-default}`. A `${` with no `}`, or with a name that is not an
/// environment variable name, is reported as malformed and left alone. Built on
/// [`split_env_references`], so the two cannot disagree about what a reference is.
pub(crate) fn scan_references(text: &str) -> Scan {
    let mut scan = Scan::default();
    for segment in split_env_references(text) {
        match segment {
            Segment::Ref(reference) => scan.refs.push(reference),
            Segment::Literal(literal) => {
                // Every `${` left in a literal is malformed: a well-formed one was cut out.
                let mut rest = literal;
                while let Some(start) = rest.find("${") {
                    let span = match rest[start..].find('}') {
                        Some(end) => &rest[start..=start + end],
                        None => &rest[start..],
                    };
                    scan.malformed.push(span.to_owned());
                    rest = &rest[start + span.len()..];
                }
            }
        }
    }
    scan
}

/// The file as serde reads it, before the checks that turn it into an [`McpConfig`].
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawMcp {
    #[serde(default, rename = "mcpServers")]
    pub(crate) servers: BTreeMap<String, RawServer>,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

/// One server entry, every key optional.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct RawServer {
    #[serde(rename = "type")]
    pub(crate) kind: Option<String>,
    pub(crate) command: Option<String>,
    pub(crate) args: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) url: Option<String>,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) tools: Option<Vec<String>>,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_references_and_defaults() {
        let s = scan_references("a ${A} b ${B:-x y} c ${ } ${1X} ${open");
        let names: Vec<_> = s.refs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["A", "B"]);
        assert_eq!(s.refs[1].default.as_deref(), Some("x y"));
        assert_eq!(s.malformed.len(), 3, "{:?}", s.malformed);
    }

    #[test]
    fn plain_text_has_none() {
        let s = scan_references("no refs $HOME {x} $");
        assert!(s.refs.is_empty() && s.malformed.is_empty());
    }

    fn reference(name: &str, default: Option<&str>) -> Segment<'static> {
        Segment::Ref(EnvRef {
            name: name.to_owned(),
            default: default.map(str::to_owned),
        })
    }

    #[test]
    fn splits_into_literals_and_references() {
        assert_eq!(
            split_env_references("Bearer ${TOKEN} at ${HOST:-localhost}:${PORT:-}!"),
            [
                Segment::Literal("Bearer "),
                reference("TOKEN", None),
                Segment::Literal(" at "),
                reference("HOST", Some("localhost")),
                Segment::Literal(":"),
                reference("PORT", Some("")),
                Segment::Literal("!"),
            ]
        );
        assert_eq!(split_env_references(""), []);
        assert_eq!(split_env_references("plain"), [Segment::Literal("plain")]);
        assert_eq!(split_env_references("${A}"), [reference("A", None)]);
        assert_eq!(
            split_env_references("${A}${B}"),
            [reference("A", None), reference("B", None)]
        );
    }

    #[test]
    fn malformed_references_stay_in_the_literal_text() {
        // Not a name, unterminated, and a `${` swallowed by an earlier malformed span.
        assert_eq!(
            split_env_references("a ${1X} b ${A} c ${ ${B} d ${open"),
            [
                Segment::Literal("a ${1X} b "),
                reference("A", None),
                Segment::Literal(" c ${ ${B} d ${open"),
            ]
        );
        // The first `}` ends a reference, so a default cannot contain one.
        assert_eq!(
            split_env_references("${A:-x}y}"),
            [reference("A", Some("x")), Segment::Literal("y}")]
        );
        // A default is taken from the first `:-`.
        assert_eq!(
            split_env_references("${A:-b:-c}"),
            [reference("A", Some("b:-c"))]
        );
    }

    /// The scanner as it was before it was built on [`split_env_references`], kept as the
    /// specification the new one must agree with.
    fn reference_scan(text: &str) -> (Vec<EnvRef>, Vec<String>) {
        let (mut refs, mut malformed) = (Vec::new(), Vec::new());
        let mut rest = text;
        while let Some(start) = rest.find("${") {
            let after = &rest[start + 2..];
            let Some(end) = after.find('}') else {
                malformed.push(rest[start..].to_owned());
                break;
            };
            let inner = &after[..end];
            let (name, default) = match inner.split_once(":-") {
                Some((n, d)) => (n, Some(d.to_owned())),
                None => (inner, None),
            };
            if is_env_name(name) {
                refs.push(EnvRef {
                    name: name.to_owned(),
                    default,
                });
            } else {
                malformed.push(format!("${{{inner}}}"));
            }
            rest = &after[end + 1..];
        }
        (refs, malformed)
    }

    proptest::proptest! {
        /// Texts made of the characters that matter to the grammar.
        #[test]
        fn the_splitter_and_the_scanner_agree(text in "[${}:\\-A-Za-z0-9_ .]{0,40}") {
            let scan = scan_references(&text);
            let (refs, malformed) = reference_scan(&text);
            proptest::prop_assert_eq!(&scan.refs, &refs);
            proptest::prop_assert_eq!(&scan.malformed, &malformed);

            // The segments are the text, cut: writing them out again gives it back.
            let segments = split_env_references(&text);
            let again: String = segments
                .iter()
                .map(|s| match s {
                    Segment::Literal(literal) => (*literal).to_owned(),
                    Segment::Ref(r) => r.written(),
                })
                .collect();
            proptest::prop_assert_eq!(&again, &text);
            proptest::prop_assert!(segments.iter().all(|s| !matches!(s, Segment::Literal(""))));
            let in_segments: Vec<&EnvRef> = segments
                .iter()
                .filter_map(|s| match s { Segment::Ref(r) => Some(r), Segment::Literal(_) => None })
                .collect();
            proptest::prop_assert_eq!(in_segments, refs.iter().collect::<Vec<_>>());
        }
    }
}
