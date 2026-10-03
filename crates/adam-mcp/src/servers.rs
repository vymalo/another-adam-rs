//! [`McpServers`]: the servers of an `mcp.json`, connected, with their tools.
//!
//! ```mermaid
//! sequenceDiagram
//!     participant A as caller (connect_mcp)
//!     participant X as expand + checks
//!     participant S as MCP server
//!     A->>X: config, Env, McpPolicy
//!     Note over X: every server, no I/O: sse refused, stdio only when allowed,<br/>${VAR} in a url refused (unless opted in), ${VAR} expanded (missing = error),<br/>URL and headers checked
//!     loop each server, in name order
//!         X->>S: spawn or dial, initialize (within connect_timeout)
//!         S-->>X: initialized
//!         X->>S: tools/list (all pages)
//!         S-->>X: tools
//!         Note over X: allow-list keeps the listed tools in its order (a missing one is an error);<br/>without one, names that do not fit are skipped with a warning
//!     end
//!     X-->>A: McpServers (tools() = <server>__<tool>)
//! ```
//!
//! A server the deployment bound to a bearer per call ([`McpPolicy::bearer_per_call`]) is checked
//! in the first pass too (its URL at the bound origin, no `Authorization` header in the file), and
//! listed with the listing bearer on a connection that is closed again; its calls are `bearer.rs`.

use std::collections::BTreeMap;
use std::sync::Arc;

use adam_agent_fs::{McpConfig, McpServer, RemoteKind, Segment, split_env_references};
use adam_llm_agent::ToolSet;
use adam_model::ToolSpec;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::Tool as ListedTool;
use secrecy::SecretString;
use serde_json::Value;

use crate::bearer::PerCall;
use crate::connection::{Connection, Recipe, Transport};
use crate::error::Error;
use crate::expand::{Env, Expander};
use crate::policy::McpPolicy;
use crate::redact::Redactor;
use crate::text::{MAX_DESCRIPTION_BYTES, cap_text};
use crate::tool::{McpTool, Target};
use crate::url;

/// The servers of an `mcp.json`, connected and listed.
///
/// [`connect`](Self::connect) is the whole startup: everything that can be wrong with the file,
/// the environment or the policy is found there, before the first model call, and a server that is
/// down is an error and not a tool that fails later. [`tools`](Self::tools) then gives the
/// tools, named `<server>__<tool>`.
///
/// The connections live as long as this value **or any tool made by [`tools`](Self::tools)**
/// lives: dropping them all closes the sessions and kills the child processes.
/// [`shutdown`](Self::shutdown) closes them now and makes every later call an error result.
/// `Debug` shows the names of the servers and tools and nothing else.
pub struct McpServers {
    servers: Vec<Server>,
}

struct Server {
    name: String,
    target: Target,
    tools: Vec<Arc<McpTool>>,
}

impl std::fmt::Debug for McpServers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut map = f.debug_map();
        for server in &self.servers {
            let names: Vec<String> = server.tools.iter().map(|t| t.spec_name()).collect();
            map.entry(&server.name, &names);
        }
        map.finish()
    }
}

impl McpServers {
    /// Connect to every server of `config`, in the order of their names, and list their tools.
    ///
    /// Two passes. First, with no process started and no request made, each server is checked:
    /// `type: sse` is refused, a local process only when [`McpPolicy::allow_stdio`], every
    /// `${VAR}` in the command, arguments, `env` and headers is expanded (from `env`, then
    /// the process environment; `${VAR:-default}` takes the default for an unset or empty
    /// variable), a `${VAR}` in the URL is refused ([`Error::UrlSecret`]) unless
    /// [`McpPolicy::allow_url_secrets`], the URL must be https (or local, unless
    /// [`McpPolicy::allow_insecure`]) with no credentials in it, and the headers must be valid. Then each server is started or dialled,
    /// initialized and asked for its tools, and the `tools:` allow-list (or, without one, every
    /// tool whose `<server>__<tool>` name fits `^[A-Za-z0-9_-]{1,64}$`) is applied.
    ///
    /// # Errors
    ///
    /// The first problem, as [`Error`]. When it comes from a later server, the servers already
    /// connected are closed (their processes killed) on the way out.
    pub async fn connect(config: &McpConfig, env: &Env, policy: &McpPolicy) -> Result<Self, Error> {
        let mut plans = Vec::with_capacity(config.servers.len());
        for (name, server) in &config.servers {
            plans.push(Plan::new(name, server, env, policy)?);
        }
        let mut servers = Vec::with_capacity(plans.len());
        for plan in plans {
            let Plan {
                recipe,
                allow,
                call_timeout,
                per_call,
            } = plan;
            let name = recipe.server.clone();
            let (target, listed) = match per_call {
                // A bound server is listed with the listing bearer, and dialled again per call.
                Some(per_call) => {
                    let per_call = Arc::new(per_call);
                    let listed = per_call.list().await?;
                    (Target::PerCall(per_call), listed)
                }
                None => {
                    let (connection, listed) = Connection::open(recipe).await?;
                    (Target::Kept(connection), listed)
                }
            };
            let selected = match select_tools(&name, listed, allow.as_deref()) {
                Ok(selected) => selected,
                Err(error) => {
                    target.close().await;
                    return Err(error);
                }
            };
            let tools = selected
                .into_iter()
                .map(|s| {
                    Arc::new(McpTool::new(
                        s.spec,
                        name.clone(),
                        s.remote,
                        s.title,
                        target.clone(),
                        call_timeout,
                    ))
                })
                .collect();
            tracing::info!(server = %name, "connected to the MCP server");
            servers.push(Server {
                name,
                target,
                tools,
            });
        }
        Ok(Self { servers })
    }

    /// The tools of every server, named `<server>__<tool>`, servers in the order of their names
    /// and each server's tools in its allow-list order (or the order the server lists them).
    pub fn tools(&self) -> ToolSet {
        self.servers
            .iter()
            .flat_map(|server| server.tools.iter())
            .fold(ToolSet::new(), |set, tool| set.dyn_tool(tool.clone()))
    }

    /// The names of the tools, as [`tools`](Self::tools) names them.
    pub fn names(&self) -> Vec<String> {
        self.tools().names()
    }

    /// Close every session now, and make every later call of a tool an error result. A child
    /// process is asked to end and killed if it has not within a few seconds.
    pub async fn shutdown(self) {
        for server in &self.servers {
            server.target.close().await;
        }
    }
}

impl McpTool {
    fn spec_name(&self) -> String {
        use adam_llm_agent::Tool as _;
        self.spec().name
    }
}

/// What the first pass decided for one server.
struct Plan {
    recipe: Recipe,
    allow: Option<Vec<String>>,
    call_timeout: std::time::Duration,
    /// Set for a remote server the deployment bound to a bearer per call.
    per_call: Option<PerCall>,
}

impl Plan {
    fn new(name: &str, server: &McpServer, env: &Env, policy: &McpPolicy) -> Result<Self, Error> {
        // `__` separates the server from the tool, so a server name cannot hold one, nor end in
        // `_` (`a_` + `x` and `a` + `_x` would both be `a___x`, and `a_`'s tools would look like
        // `a`'s). The loader of `mcp.json` refuses these too; a config made by hand is checked
        // here.
        if !name_fits(name) || name.contains("__") || name.ends_with('_') {
            return Err(Error::Name {
                server: name.to_owned(),
                tool: None,
            });
        }
        let mut redactor = Redactor::default();
        let mut expander = Expander::new(name, env, &mut redactor);
        let (command_as_written, transport, allow, per_call) = match server {
            McpServer::Stdio {
                command,
                args,
                env: declared,
                tools,
            } => {
                if !policy.stdio_allowed() {
                    return Err(Error::StdioNotAllowed {
                        server: name.to_owned(),
                    });
                }
                if policy.binding(name).is_some() {
                    tracing::warn!(
                        server = name,
                        "the deployment gives a bearer per call to an MCP server of this name, but \
                         this one is a local process (`command`): it gets none, and its own `env` \
                         is what it has"
                    );
                }
                let command_text = expander.expand(command)?;
                let mut expanded_args = Vec::with_capacity(args.len());
                for arg in args {
                    expanded_args.push(SecretString::from(expander.expand(arg)?));
                }
                let mut expanded_env = Vec::with_capacity(declared.len());
                for (key, value) in declared {
                    expanded_env.push((key.clone(), SecretString::from(expander.expand(value)?)));
                }
                (
                    command.clone(),
                    Transport::Stdio {
                        command: SecretString::from(command_text),
                        args: expanded_args,
                        env: expanded_env,
                        inherit_env: policy.inherits_env(),
                    },
                    tools.clone(),
                    None,
                )
            }
            McpServer::Remote {
                kind,
                url: url_text,
                headers,
                tools,
            } => {
                if *kind == RemoteKind::Sse {
                    return Err(Error::SseUnsupported {
                        server: name.to_owned(),
                    });
                }
                if !policy.url_secrets_allowed()
                    && let Some(var) =
                        split_env_references(url_text)
                            .into_iter()
                            .find_map(|segment| match segment {
                                Segment::Ref(reference) => Some(reference.name),
                                Segment::Literal(_) => None,
                            })
                {
                    return Err(Error::UrlSecret {
                        server: name.to_owned(),
                        var,
                    });
                }
                let expanded = expander.expand(url_text)?;
                let parsed = url::check(
                    name,
                    &expanded,
                    policy.insecure_allowed(),
                    expander.redactor(),
                )?;
                let binding = policy.binding(name);
                if let Some(binding) = binding {
                    // Before any header is expanded: a credential of the file's own, even one that
                    // is not set, would be sent beside or instead of the deployment's.
                    if !binding.matches(&parsed) {
                        let at = parsed.origin().ascii_serialization();
                        return Err(Error::BearerBinding {
                            server: name.to_owned(),
                            why: format!(
                                "the deployment gives this server a bearer only at {}, and the \
                                 file points it at {}",
                                binding.origin,
                                expander.redactor().scrub(&at)
                            ),
                        });
                    }
                    if headers
                        .keys()
                        .any(|k| k.eq_ignore_ascii_case("authorization"))
                    {
                        return Err(Error::BearerBinding {
                            server: name.to_owned(),
                            why: "the file sets an `Authorization` header, and the deployment \
                                  gives this server its own bearer for every call: remove the \
                                  header from the file"
                                .to_owned(),
                        });
                    }
                }
                let mut expanded_headers = Vec::with_capacity(headers.len());
                for (key, value) in headers {
                    let bad = || Error::Header {
                        server: name.to_owned(),
                        header: key.clone(),
                    };
                    let header_name = HeaderName::from_bytes(key.as_bytes()).map_err(|_| bad())?;
                    let header_value =
                        HeaderValue::from_str(&expander.expand(value)?).map_err(|_| bad())?;
                    expanded_headers.push((header_name, header_value));
                }
                let per_call = binding.map(|binding| {
                    PerCall::new(
                        name,
                        Arc::clone(&binding.bearer),
                        parsed.clone(),
                        expanded_headers.clone(),
                        expander.redactor().clone(),
                        policy.connect_timeout_value(),
                    )
                });
                (
                    String::new(),
                    Transport::Http {
                        url: parsed,
                        headers: expanded_headers,
                    },
                    tools.clone(),
                    per_call,
                )
            }
        };
        Ok(Self {
            recipe: Recipe {
                server: name.to_owned(),
                command_as_written,
                transport,
                redactor: Arc::new(redactor),
                connect_timeout: policy.connect_timeout_value(),
            },
            allow,
            call_timeout: policy.call_timeout_value(),
            per_call,
        })
    }
}

/// What the model may be shown as a tool name: `^[A-Za-z0-9_-]{1,64}$`.
fn name_fits(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether the tool `tool` of `server` can be shown as `<server>__<tool>`: the name fits, and the
/// tool's own name does not start with `_` (with a server name that does not end in one, the first
/// `__` then always ends the server's name, and a name says whose tool it is).
fn tool_fits(server: &str, tool: &str) -> bool {
    !tool.is_empty() && !tool.starts_with('_') && name_fits(&format!("{server}__{tool}"))
}

/// A tool chosen from a server's list: its name on the server and what the model is shown.
#[derive(Debug)]
pub(crate) struct Selected {
    pub(crate) remote: String,
    pub(crate) spec: ToolSpec,
    /// The tool's own human title (MCP's `title`), when the server gave one: what its step is called.
    pub(crate) title: Option<String>,
}

/// Choose the tools of `server` from what it `listed`.
///
/// With an allow-list, exactly the listed tools, in the allow-list's order; one the server lacks
/// is [`Error::UnknownTool`]. Without one, every tool whose `<server>__<tool>` fits the model's
/// name rules (and whose own name does not start with `_`); the others are skipped with a warning (a server cannot break startup by having
/// a tool with a dot in its name), and so is a repeated name.
pub(crate) fn select_tools(
    server: &str,
    listed: Vec<ListedTool>,
    allow: Option<&[String]>,
) -> Result<Vec<Selected>, Error> {
    let mut by_name: BTreeMap<String, ListedTool> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for tool in listed {
        let name = tool.name.to_string();
        if by_name.contains_key(&name) {
            tracing::warn!(server, tool = %name, "the MCP server lists a tool twice; the first is used");
            continue;
        }
        order.push(name.clone());
        by_name.insert(name, tool);
    }
    let chosen: Vec<String> = match allow {
        Some(allow) => {
            let mut chosen = Vec::with_capacity(allow.len());
            for wanted in allow {
                if !by_name.contains_key(wanted) {
                    return Err(Error::UnknownTool {
                        server: server.to_owned(),
                        tool: wanted.clone(),
                        available: order,
                    });
                }
                if !tool_fits(server, wanted) {
                    return Err(Error::Name {
                        server: server.to_owned(),
                        tool: Some(wanted.clone()),
                    });
                }
                if !chosen.contains(wanted) {
                    chosen.push(wanted.clone());
                }
            }
            chosen
        }
        None => order
            .into_iter()
            .filter(|name| {
                let fits = tool_fits(server, name);
                if !fits {
                    tracing::warn!(
                        server,
                        tool = %name,
                        "skipping an MCP tool whose name cannot be shown to a model as \
                         `<server>__<tool>` (letters, digits, `-` and `_`; at most 64 characters; \
                         a tool name does not start with `_`); list the tools you want under \
                         `tools:` to see this only when it matters"
                    );
                }
                fits
            })
            .collect(),
    };
    Ok(chosen
        .into_iter()
        .filter_map(|name| by_name.remove(&name))
        .map(|tool| selected(server, tool))
        .collect())
}

fn selected(server: &str, tool: ListedTool) -> Selected {
    let remote = tool.name.to_string();
    let title = tool
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    let description = tool
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            tool.title
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("`{remote}` from the MCP server `{server}`."));
    let mut schema: serde_json::Map<String, Value> = (*tool.input_schema).clone();
    schema
        .entry("type")
        .or_insert_with(|| Value::String("object".to_owned()));
    Selected {
        spec: ToolSpec {
            name: format!("{server}__{remote}"),
            description: cap_text(description, MAX_DESCRIPTION_BYTES),
            parameters: Value::Object(schema),
        },
        remote,
        title,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;

    fn listed(name: &str) -> ListedTool {
        let schema = json!({"type": "object", "properties": {"q": {"type": "string"}}});
        let Value::Object(schema) = schema else {
            unreachable!()
        };
        ListedTool::new(
            name.to_owned(),
            format!("{name} description"),
            Arc::new(schema),
        )
    }

    fn names(selected: &[Selected]) -> Vec<&str> {
        selected.iter().map(|s| s.spec.name.as_str()).collect()
    }

    #[test]
    #[allow(non_snake_case)] // the name is the rule: `<server>__<tool>`
    fn names_are_server__tool() {
        let chosen =
            select_tools("linear", vec![listed("list_issues"), listed("get")], None).unwrap();
        assert_eq!(names(&chosen), ["linear__list_issues", "linear__get"]);
        assert_eq!(chosen[0].remote, "list_issues");
        assert_eq!(chosen[0].spec.description, "list_issues description");
    }

    #[test]
    fn allow_list_keeps_the_listed_tools_in_its_order() {
        let allow = ["get".to_owned(), "list_issues".to_owned()];
        let chosen = select_tools(
            "linear",
            vec![listed("list_issues"), listed("delete_all"), listed("get")],
            Some(&allow),
        )
        .unwrap();
        assert_eq!(names(&chosen), ["linear__get", "linear__list_issues"]);

        let allow = ["get".to_owned(), "gett".to_owned()];
        let error = select_tools("linear", vec![listed("get")], Some(&allow)).unwrap_err();
        assert!(matches!(
            &error,
            Error::UnknownTool { tool, available, .. } if tool == "gett" && available == &["get"]
        ));
    }

    #[test]
    fn unmappable_names_skipped_without_allow_list() {
        let chosen = select_tools(
            "linear",
            vec![
                listed("a.b"),
                listed("ok"),
                listed("has space"),
                listed("ok"),
            ],
            None,
        )
        .unwrap();
        assert_eq!(names(&chosen), ["linear__ok"]);
        // The 64-character limit counts the server name and the `__`.
        let long = "t".repeat(60);
        assert!(
            select_tools("linear", vec![listed(&long)], None)
                .unwrap()
                .is_empty()
        );
        // With an allow-list, an unmappable name is an error, not a silent skip.
        let allow = ["a.b".to_owned()];
        let error = select_tools("linear", vec![listed("a.b")], Some(&allow)).unwrap_err();
        assert!(matches!(error, Error::Name { tool: Some(_), .. }));
    }

    #[test]
    fn a_tool_name_starting_with_an_underscore_cannot_be_told_from_another_server() {
        // `a` + `_x` and `a_` + `x` would both be `a___x`: a tool that starts with `_` is not mapped.
        let chosen = select_tools("a", vec![listed("_x"), listed("y"), listed("_")], None).unwrap();
        assert_eq!(names(&chosen), ["a__y"]);
        let allow = ["_x".to_owned()];
        let error = select_tools("a", vec![listed("_x")], Some(&allow)).unwrap_err();
        assert!(
            matches!(&error, Error::Name { server, tool: Some(tool) } if server == "a" && tool == "_x"),
            "{error}"
        );
        // An underscore inside a tool name is fine.
        let chosen = select_tools("a", vec![listed("x_y")], None).unwrap();
        assert_eq!(names(&chosen), ["a__x_y"]);
    }

    #[test]
    fn schema_passthrough_adds_type_object() {
        let bare = ListedTool::new_with_raw("t".to_owned(), None, Arc::new(serde_json::Map::new()));
        let chosen = select_tools("s", vec![bare], None).unwrap();
        assert_eq!(chosen[0].spec.parameters, json!({"type": "object"}));
        // A schema that has one is passed through untouched.
        let chosen = select_tools("s", vec![listed("t")], None).unwrap();
        assert_eq!(
            chosen[0].spec.parameters,
            json!({"type": "object", "properties": {"q": {"type": "string"}}})
        );
    }

    #[test]
    fn the_title_is_kept_for_the_step_and_blank_is_none() {
        let with = |title: &str| {
            ListedTool::new_with_raw("t".to_owned(), None, Arc::new(serde_json::Map::new()))
                .with_title(title.to_owned())
        };
        let chosen = select_tools("s", vec![with("  Search the web ")], None).unwrap();
        assert_eq!(chosen[0].title.as_deref(), Some("Search the web"));
        let chosen = select_tools("s", vec![with("   ")], None).unwrap();
        assert_eq!(chosen[0].title, None);
        let chosen = select_tools("s", vec![listed("t")], None).unwrap();
        assert_eq!(chosen[0].title, None);
    }

    #[test]
    fn description_falls_back_to_the_title_then_to_a_default() {
        let titled =
            ListedTool::new_with_raw("t".to_owned(), None, Arc::new(serde_json::Map::new()))
                .with_title("A title");
        let plain =
            ListedTool::new_with_raw("u".to_owned(), None, Arc::new(serde_json::Map::new()));
        let long = ListedTool::new(
            "v".to_owned(),
            "d".repeat(20_000),
            Arc::new(serde_json::Map::new()),
        );
        let chosen = select_tools("s", vec![titled, plain, long], None).unwrap();
        assert_eq!(chosen[0].spec.description, "A title");
        assert_eq!(chosen[1].spec.description, "`u` from the MCP server `s`.");
        assert!(chosen[2].spec.description.len() < 9_000);
        assert!(chosen[2].spec.description.contains("[cut here"));
    }

    fn plan_error(server: McpServer, policy: &McpPolicy) -> Error {
        match Plan::new("srv", &server, &Env::new(), policy) {
            Err(error) => error,
            Ok(_) => panic!("expected a refusal"),
        }
    }

    fn remote(kind: RemoteKind, url: &str) -> McpServer {
        McpServer::Remote {
            kind,
            url: url.to_owned(),
            headers: Default::default(),
            tools: None,
        }
    }

    #[test]
    fn sse_is_refused() {
        let error = plan_error(
            remote(RemoteKind::Sse, "https://x.example.com/sse"),
            &McpPolicy::default(),
        );
        assert!(matches!(error, Error::SseUnsupported { .. }), "{error}");
    }

    #[test]
    fn stdio_refused_before_spawn() {
        let stdio = McpServer::Stdio {
            command: "definitely-not-a-real-command".to_owned(),
            args: vec![],
            env: Default::default(),
            tools: None,
        };
        // The refusal is a plan error: nothing was started, and the command was never looked up.
        let error = plan_error(stdio, &McpPolicy::default());
        assert!(matches!(error, Error::StdioNotAllowed { .. }), "{error}");
    }

    #[test]
    fn errors_scrubbed_of_expanded_values() {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("Authorization".to_owned(), "Bearer ${MCP_TOKEN}".to_owned());
        let server = McpServer::Remote {
            kind: RemoteKind::Http,
            url: "https://x.example.com/mcp?key=${MCP_KEY}".to_owned(),
            headers,
            tools: None,
        };
        let env = Env::new()
            .var("MCP_TOKEN", "tok-4c1e9d7a")
            .var("MCP_KEY", "key-88b2f0e3");
        // A variable in the URL needs the opt-in.
        let policy = McpPolicy::default().allow_url_secrets(true);
        let plan = Plan::new("srv", &server, &env, &policy).unwrap();
        let scrubbed = plan.recipe.scrub(
            "error sending request for url (https://x.example.com/mcp?key=key-88b2f0e3): \
             sent Bearer tok-4c1e9d7a, then tok-4c1e9d7a alone",
        );
        for leaked in ["tok-4c1e9d7a", "key-88b2f0e3"] {
            assert!(!scrubbed.contains(leaked), "{scrubbed}");
        }
        assert!(scrubbed.contains("[REDACTED]"), "{scrubbed}");
        assert!(scrubbed.contains("error sending request"), "{scrubbed}");
    }

    #[test]
    fn a_variable_in_the_url_is_refused_unless_the_policy_allows_it() {
        let env = Env::new()
            .var("MCP_KEY", "key-88b2f0e3")
            .var("MCP_URL", "https://x.example.com/mcp");
        for url in [
            "https://x.example.com/mcp?key=${MCP_KEY}",
            "https://x.example.com/${MCP_KEY}/mcp",
            "${MCP_URL}",
            // Even a reference with a default, and one whose variable is not set: what a
            // deployment may set later is what the file must not put in a URL.
            "https://x.example.com/mcp?v=${MCP_SURELY_UNSET_1D5C:-2}",
        ] {
            let server = remote(RemoteKind::Http, url);
            let error = Plan::new("srv", &server, &env, &McpPolicy::default())
                .err()
                .unwrap_or_else(|| panic!("`{url}` was accepted"));
            assert!(
                matches!(&error, Error::UrlSecret { server, var }
                    if server == "srv" && var.starts_with("MCP_")),
                "{url}: {error}"
            );
            let shown = format!("{error} / {error:?}");
            assert!(!shown.contains("key-88b2f0e3"), "{shown}");
            assert!(shown.contains("MCP_"), "{shown}");
            assert_eq!(
                adam_error::Classify::class(&error),
                adam_error::ErrorClass::Invalid
            );
            assert!(
                Plan::new(
                    "srv",
                    &server,
                    &env,
                    &McpPolicy::default().allow_url_secrets(true)
                )
                .is_ok(),
                "{url} with the opt-in"
            );
        }
        // A variable in a header is what to use, and needs no opt-in.
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("X-Api-Key".to_owned(), "${MCP_KEY}".to_owned());
        let server = McpServer::Remote {
            kind: RemoteKind::Http,
            url: "https://x.example.com/mcp".to_owned(),
            headers,
            tools: None,
        };
        assert!(Plan::new("srv", &server, &env, &McpPolicy::default()).is_ok());
    }

    #[test]
    fn url_errors_do_not_show_a_secret_in_the_path() {
        // With the opt-in a secret may be in the path or the host, and an error that prints the URL
        // must not print it: plain or percent-encoded, in an insecure, a non-http or a bad URL.
        let env = Env::new()
            .var("MCP_KEY", "key-88b2f0e3")
            .var("MCP_SPACED", "a b/ü");
        let policy = McpPolicy::default().allow_url_secrets(true);
        for url in [
            "http://x.example.com/v1/${MCP_KEY}/mcp",
            "ftp://${MCP_KEY}.example.com/mcp",
            "http://x.example.com/${MCP_SPACED}/mcp",
        ] {
            let error = Plan::new("srv", &remote(RemoteKind::Http, url), &env, &policy)
                .err()
                .unwrap_or_else(|| panic!("`{url}` was accepted"));
            assert!(matches!(error, Error::Url { .. }), "{url}: {error}");
            let shown = format!("{error} / {error:?}");
            assert!(shown.contains("[REDACTED]"), "{shown}");
            for leaked in ["key-88b2f0e3", "a%20b", "a b", "%C3%BC", "ü"] {
                assert!(!shown.contains(leaked), "{leaked} in {shown}");
            }
        }
    }

    #[test]
    fn bad_headers_and_names_are_refused_without_their_values() {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert(
            "Authorization".to_owned(),
            "Bearer bad\nvalue-sekrit".to_owned(),
        );
        let server = McpServer::Remote {
            kind: RemoteKind::Http,
            url: "https://x.example.com/mcp".to_owned(),
            headers,
            tools: None,
        };
        let error = plan_error(server, &McpPolicy::default());
        assert!(matches!(&error, Error::Header { header, .. } if header == "Authorization"));
        assert!(!error.to_string().contains("sekrit"));

        let error = Plan::new(
            "bad name",
            &remote(RemoteKind::Http, "https://x.example.com/mcp"),
            &Env::new(),
            &McpPolicy::default(),
        )
        .err()
        .unwrap();
        assert!(matches!(error, Error::Name { tool: None, .. }));
        let error = Plan::new(
            "a__b",
            &remote(RemoteKind::Http, "https://x.example.com/mcp"),
            &Env::new(),
            &McpPolicy::default(),
        )
        .err()
        .unwrap();
        assert!(matches!(error, Error::Name { tool: None, .. }));
        // `a` + `_x` and `a_` + `x` would both be `a___x`: no server name ends in `_`.
        let error = Plan::new(
            "a_",
            &remote(RemoteKind::Http, "https://x.example.com/mcp"),
            &Env::new(),
            &McpPolicy::default(),
        )
        .err()
        .unwrap();
        assert!(matches!(error, Error::Name { tool: None, .. }));
        // An underscore inside a name is fine.
        assert!(
            Plan::new(
                "a_b",
                &remote(RemoteKind::Http, "https://x.example.com/mcp"),
                &Env::new(),
                &McpPolicy::default(),
            )
            .is_ok()
        );
    }
}
