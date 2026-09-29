//! The errors of connecting to the servers of an `mcp.json`. A closed enum: every variant names
//! the server, and none carries a value that came from an environment variable.

use std::fmt;

use adam_error::{Classify, ErrorClass};

/// What is wrong with an environment variable a `mcp.json` refers to, in [`Error::Var`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarProblem {
    /// Not set, in [`Env`](crate::Env) or in the process environment, and the reference has no
    /// `:-default`.
    Missing,
    /// Set, and not valid Unicode.
    NotUnicode,
}

impl fmt::Display for VarProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Missing => "is not set",
            Self::NotUnicode => "is not valid Unicode",
        })
    }
}

/// Why the URL of a remote server was refused, in [`Error::Url`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlProblem {
    /// Not a URL, after `${VAR}` was expanded.
    Unparseable,
    /// Not `http` or `https`, or no host.
    NotHttp,
    /// A user name or a password in the URL: credentials go in `headers`.
    Credentials,
    /// Plain `http` to a host that is not this machine, and
    /// [`McpPolicy::allow_insecure`](crate::McpPolicy::allow_insecure) is off.
    Insecure,
}

impl fmt::Display for UrlProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unparseable => "it is not a URL",
            Self::NotHttp => "it must be an http(s) URL with a host",
            Self::Credentials => {
                "it carries a user name or password: put credentials in `headers` and refer to \
                 them as `${VAR}`"
            }
            Self::Insecure => {
                "plain http to a host that is not this machine sends every request in the clear: \
                 use https (or, for development only, McpPolicy::allow_insecure)"
            }
        })
    }
}

/// Why the servers of an `mcp.json` could not be connected. Found when the process starts, and
/// never carries the value of an environment variable.
///
/// [`Connect`](Self::Connect), [`Spawn`](Self::Spawn) and [`ListTools`](Self::ListTools) are
/// [`ErrorClass::Transient`]: the server may be up later. Every other variant is
/// [`ErrorClass::Invalid`]: the same files and policy never succeed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A `${VAR}` names a variable that has no value.
    #[error(
        "MCP server `{server}`: the environment variable `{var}` {problem}: set it, or write a \
         default as `${{{var}:-default}}`"
    )]
    Var {
        /// The server.
        server: String,
        /// The variable's name (never its value).
        var: String,
        /// What is wrong with it.
        problem: VarProblem,
    },
    /// A stdio server, and [`McpPolicy::allow_stdio`](crate::McpPolicy::allow_stdio) is off.
    /// Nothing was spawned.
    #[error(
        "MCP server `{server}` is a local process (`command`), which this deployment does not \
         allow: opt in with McpPolicy::allow_stdio, or use a remote server (`type: http`)"
    )]
    StdioNotAllowed {
        /// The server.
        server: String,
    },
    /// `type: sse`: the HTTP+SSE transport is deprecated by the MCP specification and the client
    /// does not speak it.
    #[error(
        "MCP server `{server}` is `type: sse`, the deprecated HTTP+SSE transport, which is not \
         supported: use `type: http` (streamable HTTP) if the server offers it"
    )]
    SseUnsupported {
        /// The server.
        server: String,
    },
    /// The URL of a remote server cannot be used.
    #[error("MCP server `{server}`: the URL {url} is refused: {problem}")]
    Url {
        /// The server.
        server: String,
        /// The URL as it can be shown: without a user name, password, query or fragment.
        url: String,
        /// Why.
        problem: UrlProblem,
    },
    /// A `${VAR}` in the `url` of a remote server, and
    /// [`McpPolicy::allow_url_secrets`](crate::McpPolicy::allow_url_secrets) is off. The SDK logs
    /// the URL it dials (in its own log lines, which the redactor of this crate cannot reach), so
    /// nothing that came from a variable may be in it unless the deployment says it filters those
    /// logs. Nothing was requested.
    #[error(
        "MCP server `{server}`: the `url` refers to the environment variable `{var}`: a URL is \
         logged by the MCP library, so credentials go in `headers` (`Authorization: Bearer \
         ${{{var}}}`); or opt in with McpPolicy::allow_url_secrets and filter the `rmcp` log \
         target"
    )]
    UrlSecret {
        /// The server.
        server: String,
        /// The variable's name (never its value).
        var: String,
    },
    /// A header of a remote server is not a valid header (name or value).
    #[error("MCP server `{server}`: the header `{header}` is not a valid HTTP header")]
    Header {
        /// The server.
        server: String,
        /// The header's name.
        header: String,
    },
    /// A server or tool name cannot be shown to a model as `<server>__<tool>` (letters, digits,
    /// `-` and `_`; at most 64 characters; a server name does not end in `_` and a tool name does
    /// not start with one, so that the first `__` always ends the server's name).
    #[error(
        "MCP server `{server}`: {} cannot be named for the model as `<server>__<tool>` (letters, \
         digits, `-` and `_`; at most 64 characters; a server name does not end in `_` and a tool \
         name does not start with `_`)",
        .tool.as_deref().map_or_else(|| "the server name".to_owned(), |t| format!("the tool `{t}`"))
    )]
    Name {
        /// The server.
        server: String,
        /// The tool, when it is the tool's name that does not fit.
        tool: Option<String>,
    },
    /// The process of a stdio server could not be started.
    #[error("MCP server `{server}`: cannot start `{command}`: {message}")]
    Spawn {
        /// The server.
        server: String,
        /// The command as written in the file (before `${VAR}` expansion).
        command: String,
        /// Why, without any expanded value.
        message: String,
    },
    /// The server did not answer the handshake: down, refused the credentials, or too slow.
    #[error("MCP server `{server}`: cannot connect: {message}")]
    Connect {
        /// The server.
        server: String,
        /// Why, without any expanded value.
        message: String,
    },
    /// The server connected and did not list its tools.
    #[error("MCP server `{server}`: cannot list its tools: {message}")]
    ListTools {
        /// The server.
        server: String,
        /// Why, without any expanded value.
        message: String,
    },
    /// The allow-list (`tools:`) names a tool the server does not have.
    #[error(
        "MCP server `{server}`: `tools` names `{tool}`, which the server does not offer; it \
         offers: {}",
        list(.available)
    )]
    UnknownTool {
        /// The server.
        server: String,
        /// The tool as written.
        tool: String,
        /// The tools the server lists, in its order.
        available: Vec<String>,
    },
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_owned()
    } else {
        items
            .iter()
            .map(|i| format!("`{i}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl Classify for Error {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Connect { .. } | Self::Spawn { .. } | Self::ListTools { .. } => {
                ErrorClass::Transient
            }
            Self::Var { .. }
            | Self::StdioNotAllowed { .. }
            | Self::SseUnsupported { .. }
            | Self::Url { .. }
            | Self::UrlSecret { .. }
            | Self::Header { .. }
            | Self::Name { .. }
            | Self::UnknownTool { .. } => ErrorClass::Invalid,
        }
    }
}
