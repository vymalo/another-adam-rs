//! `mcp.json`: the `mcpServers` shape of Claude Code's `.mcp.json` and the draft SEP-2633.
//!
//! `${VAR}` and `${VAR:-default}` are **never** expanded here: expansion happens at run time,
//! from the process environment, so that a secret never passes through the build. This module
//! only finds the references ([`McpConfig::env_references`]) so a build can record their names.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::names::is_env_name;

/// The most keywords a `when` may hold.
pub const MAX_WHEN_KEYWORDS: usize = 16;

/// The longest keyword a `when` may hold, in characters.
pub const MAX_WHEN_KEYWORD_CHARS: usize = 64;

/// The most characters a `when` server's name may hold: its name becomes its group's name, and a
/// group's name is at most 40 characters (ADR 0066).
pub const MAX_GROUP_NAME_CHARS: usize = 40;

/// Whether `name` can be a group's name: `^[a-z0-9]+(_[a-z0-9]+)*$`, at most
/// [`MAX_GROUP_NAME_CHARS`] characters. A `when` server's name becomes its group's name, so the
/// loader refuses a `when` server whose name does not fit; nothing is renamed, because a rename
/// would change its tools' `<server>__<tool>` names.
pub fn group_name_fits(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_GROUP_NAME_CHARS
        && name.split('_').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// `when` (an adam extension): the condition under which a server's tools are offered to the
/// model.
///
/// A server with a `when` is one **group** of tools (ADR 0066). Its tools are withheld from a run
/// whose task mentions none of [`keywords`](Self::keywords), ignoring case; the task is the same
/// text at every turn, so the test is recomputed and does not move. A run that needs the group
/// loads it with its `load__group__<server>` loader, which appends its tools to the next request
/// (ADR 0044, decision 1). The server's name becomes the group's name, so it must be lowercase
/// letters and digits with single underscores, at most 40 characters
/// ([`group_name_fits`](crate::group_name_fits)); a server without `when` is loaded for every run,
/// as before the key existed.
///
/// There are no paths in this version: the coder has no checkout before its first model call
/// (`prepare_workspace` is a tool), so a path glob would have nothing to match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpWhen {
    /// The words, any one of which loads the server's tools when the task holds it, ignoring case:
    /// 1 to `MAX_WHEN_KEYWORDS`, each 1 to `MAX_WHEN_KEYWORD_CHARS` characters, no control or
    /// format character.
    pub keywords: Vec<String>,
}

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
        /// `optional: true` (an adam extension): a server that cannot be reached at startup is
        /// skipped with a warning instead of stopping the process.
        optional: bool,
        /// `files: true` (an adam extension): the images, audio clips and blobs of its results are
        /// shared with the person as files of the run, instead of being described to the model.
        /// Left out of the JSON when `false`, so the digest of a folder that does not use it is the
        /// one it had before the key existed.
        #[serde(skip_serializing_if = "is_false")]
        files: bool,
        /// The per-server settings of the adam extensions `timeoutSecs`, `connectTimeoutSecs`,
        /// `instructions` and `prompts`. Left out of the JSON when they are all as they are
        /// without the keys, so the digest of a folder that does not use them is the one it had
        /// before they existed.
        #[serde(skip_serializing_if = "McpSettings::is_default")]
        settings: McpSettings,
        /// `when` (an adam extension): the server's tools are loaded only when the run's task
        /// mentions one of its keywords. Left out of the JSON when absent, so the digest of a
        /// folder that does not use it is the one it had before the key existed.
        #[serde(skip_serializing_if = "Option::is_none")]
        when: Option<McpWhen>,
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
        /// `optional: true` (an adam extension): a server that cannot be reached at startup is
        /// skipped with a warning instead of stopping the process.
        optional: bool,
        /// `files: true` (an adam extension): the images, audio clips and blobs of its results are
        /// shared with the person as files of the run, instead of being described to the model.
        /// Left out of the JSON when `false`, so the digest of a folder that does not use it is the
        /// one it had before the key existed.
        #[serde(skip_serializing_if = "is_false")]
        files: bool,
        /// The per-server settings of the adam extensions `timeoutSecs`, `connectTimeoutSecs`,
        /// `instructions` and `prompts`. Left out of the JSON when they are all as they are
        /// without the keys, so the digest of a folder that does not use them is the one it had
        /// before they existed.
        #[serde(skip_serializing_if = "McpSettings::is_default")]
        settings: McpSettings,
        /// `when` (an adam extension): the server's tools are loaded only when the run's task
        /// mentions one of its keywords. Left out of the JSON when absent, so the digest of a
        /// folder that does not use it is the one it had before the key existed.
        #[serde(skip_serializing_if = "Option::is_none")]
        when: Option<McpWhen>,
    },
}

/// What an `mcp.json` entry says about timeouts, the server's own instructions and its prompts
/// (adam extensions; every key is optional).
///
/// The seconds are what the **file** asks for: the deployment caps them (`MCP_MAX_CALL_SECS`), so
/// an agent's files never decide alone how long a call may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct McpSettings {
    /// `timeoutSecs`: how long one tool call of this server is waited for. `None` is the
    /// deployment's default (60 s). At least 1.
    pub timeout_secs: Option<u64>,
    /// `connectTimeoutSecs`: how long the server may take to start, initialize and list its tools.
    /// `None` is the deployment's default (30 s). At least 1.
    pub connect_timeout_secs: Option<u64>,
    /// `instructions: false` turns off the text the server returns when it initializes (it is
    /// appended to the agent's instructions once, at bind time). On by default.
    pub instructions: bool,
    /// `prompts: false` stops the server's prompts from becoming commands. On by default.
    pub prompts: bool,
    /// `parallel: false` makes the calls of this server's tools run one at a time, alone in their
    /// turn (ADR 0049); by default they may run at the same time as the turn's other calls.
    pub parallel: bool,
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            timeout_secs: None,
            connect_timeout_secs: None,
            instructions: true,
            prompts: true,
            parallel: true,
        }
    }
}

impl McpSettings {
    /// Whether every key is as it is when the file does not say it.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl McpServer {
    /// The timeouts, instructions and prompts keys of the entry.
    pub fn settings(&self) -> &McpSettings {
        match self {
            Self::Stdio { settings, .. } | Self::Remote { settings, .. } => settings,
        }
    }

    /// Whether the server may be missing: `optional: true` in the file.
    pub fn is_optional(&self) -> bool {
        match self {
            Self::Stdio { optional, .. } | Self::Remote { optional, .. } => *optional,
        }
    }

    /// Whether the files of its results are shared: `files: true` in the file.
    pub fn shares_files(&self) -> bool {
        match self {
            Self::Stdio { files, .. } | Self::Remote { files, .. } => *files,
        }
    }

    /// The allow-list of tools, when there is one.
    pub fn tools(&self) -> Option<&[String]> {
        match self {
            Self::Stdio { tools, .. } | Self::Remote { tools, .. } => tools.as_deref(),
        }
    }

    /// `when` (an adam extension): the server's tools are loaded only when the run's task mentions
    /// one of its keywords. `None` loads them for every run.
    pub fn when(&self) -> Option<&McpWhen> {
        match self {
            Self::Stdio { when, .. } | Self::Remote { when, .. } => when.as_ref(),
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

fn is_false(value: &bool) -> bool {
    !*value
}

/// A parsed `mcp.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct McpConfig {
    /// The servers by name. The model sees their tools as `<server>__<tool>`.
    pub servers: BTreeMap<String, McpServer>,
}

impl McpConfig {
    /// This config with the servers of `extra` added. Server names must be different: a name in
    /// both is returned (sorted) and nothing is merged, so a file added over the agent's own
    /// `mcp.json` can never replace one of its servers.
    ///
    /// # Errors
    ///
    /// The names that both configs have.
    pub fn merged_with(mut self, extra: McpConfig) -> Result<McpConfig, Vec<String>> {
        let clashes: Vec<String> = extra
            .servers
            .keys()
            .filter(|name| self.servers.contains_key(*name))
            .cloned()
            .collect();
        if !clashes.is_empty() {
            return Err(clashes);
        }
        self.servers.extend(extra.servers);
        Ok(self)
    }

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
    pub(crate) optional: Option<bool>,
    pub(crate) files: Option<bool>,
    #[serde(rename = "timeoutSecs")]
    pub(crate) timeout_secs: Option<Value>,
    #[serde(rename = "connectTimeoutSecs")]
    pub(crate) connect_timeout_secs: Option<Value>,
    pub(crate) instructions: Option<bool>,
    pub(crate) prompts: Option<bool>,
    pub(crate) parallel: Option<bool>,
    pub(crate) when: Option<Value>,
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
