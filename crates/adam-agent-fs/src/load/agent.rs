//! Agent, subagent and remote-subagent files.

use url::Url;

use super::{Front, read_front};
use crate::diagnostic::Sink;
use crate::frontmatter::{Split, clean_body, key_line};
use crate::manifest::RemoteAuth;
use crate::schema::{
    AgentFrontmatter, DEFAULT_RETRY_BACKOFF, MAX_MINUTES_LIMIT, MAX_OPEN_SESSIONS_LIMIT,
    MAX_PARALLEL_LIMIT, MAX_RETRY_ATTEMPTS, MAX_RETRY_BACKOFF, MAX_SESSION_ROUNDS_LIMIT,
    MAX_WORKING_SESSIONS_LIMIT, ModelRef, RETRY_CLASS_NAMES, ToolList, check_references,
    is_agent_name, is_env_name, is_retry_tool_name, is_tool_name, retry_duration,
};

/// The most characters GitHub Copilot accepts in an agent prompt. Verified 2026-09-29,
/// <https://docs.github.com/en/copilot/reference/custom-agents-configuration>.
const COPILOT_PROMPT_CAP: usize = 30_000;

/// Keys of Claude Code, Copilot and OpenCode agent files that adam reads and does not act on.
const IGNORED_KEYS: &[&str] = &[
    "argument-hint",
    "background",
    "color",
    "disable-model-invocation",
    "disallowedTools",
    "effort",
    "handoffs",
    "hooks",
    "infer",
    "initialPrompt",
    "isolation",
    "memory",
    "mode",
    "permission",
    "permissionMode",
    "target",
    "temperature",
    "top_p",
    "user-invocable",
];

/// Frontmatter keys that would put a secret or an endpoint in a file. Compared after
/// lowercasing and dropping `_` and `-`, so `apiKey`, `api_key` and `api-key` are one.
const SECRET_KEYS: &[&str] = &["apikey", "token", "secret", "password", "baseurl"];

/// Where an agent file sits, which decides how its name is found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileKind {
    /// `agent/instructions.md`: the name is the frontmatter's, else the composition root's.
    Root,
    /// `agents/<name>/instructions.md`: the name is the directory's, and the frontmatter may
    /// only repeat it.
    Named,
    /// A subagent (flat file or directory): the frontmatter's name wins, else the path's.
    Sub,
}

/// A remote subagent's target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteSpec {
    pub(crate) url: String,
    pub(crate) auth: Option<RemoteAuth>,
    pub(crate) files: bool,
}

/// One agent file, read.
#[derive(Debug)]
pub(crate) struct AgentFile {
    pub(crate) name: String,
    pub(crate) frontmatter: AgentFrontmatter,
    pub(crate) body: String,
    /// `Some` when a subagent file has `a2a:`.
    pub(crate) remote: Option<RemoteSpec>,
}

/// Read one agent file. `path_name` is the name the path gives (the directory, the file stem
/// without `.agent`, the composition root's default). `None` means the file is unusable and an
/// error was reported.
pub(crate) fn parse_agent_file(
    sink: &mut Sink<'_>,
    text: &str,
    kind: FileKind,
    path_name: Option<&str>,
) -> Option<AgentFile> {
    let Front {
        value: mut fm,
        split,
    } = read_front::<AgentFrontmatter>(sink, text)?;

    if fm.fold_claude_max_turns() {
        sink.warn(
            key_line(&split, "maxTurns"),
            "`maxTurns` and `limits.max_turns` disagree; `limits.max_turns` wins",
        );
    }
    let name = resolve_name(sink, &split, &fm, kind, path_name)?;
    check_keys(sink, &split, &fm);
    check_values(sink, &split, &fm, kind);
    check_retry(sink, &split, &fm);

    if kind != FileKind::Sub
        && fm
            .limits
            .as_ref()
            .is_some_and(|limits| limits.max_minutes.is_some())
    {
        sink.warn(
            key_line(&split, "limits"),
            "`limits.max_minutes` is for a subagent run (the parent stops it); it is ignored on this agent",
        );
    }
    if kind == FileKind::Sub
        && let Some(limits) = fm.limits.as_ref()
    {
        let keys: Vec<&str> = [
            ("`limits.max_open_sessions`", limits.max_open_sessions),
            ("`limits.max_working_sessions`", limits.max_working_sessions),
            ("`limits.max_session_rounds`", limits.max_session_rounds),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|_| key))
        .collect();
        if !keys.is_empty() {
            sink.warn(
                key_line(&split, "limits"),
                format!(
                    "the session limits ({}) are the root agent's: ignored on a subagent",
                    keys.join(", ")
                ),
            );
        }
    }
    if kind != FileKind::Sub {
        if fm.a2a.is_some() || fm.auth.is_some() {
            sink.error(
                key_line(&split, "a2a").or_else(|| key_line(&split, "auth")),
                "`a2a` and `auth` make a remote subagent; they cannot be used on an agent",
            );
        }
    } else {
        if fm.card.is_some() {
            sink.warn(
                key_line(&split, "card"),
                "`card` is for the root agent; it is ignored on a subagent",
            );
        }
        if !fm.references.is_empty() {
            sink.warn(
                key_line(&split, "references"),
                "`references` is for the root agent; it is ignored on a subagent",
            );
        }
    }

    if fm.files.is_some() && fm.a2a.is_none() {
        sink.warn(
            key_line(&split, "files"),
            "`files` is for a remote subagent (`a2a`) and is ignored here; an MCP server takes \
             `\"files\": true` in `mcp.json`",
        );
    }
    let remote = if kind == FileKind::Sub {
        remote_spec(sink, &split, &fm)
    } else {
        None
    };
    if kind == FileKind::Sub
        && fm
            .description
            .as_deref()
            .is_none_or(|d| d.trim().is_empty())
    {
        sink.error(
            key_line(&split, "description").or(Some(1)),
            format!("subagent `{name}` needs a `description`: it is what the parent agent reads"),
        );
        return None;
    }

    let body = clean_body(split.body);
    if kind == FileKind::Sub && fm.a2a.is_none() && body.is_empty() {
        sink.error(
            Some(split.body_line),
            format!("subagent `{name}` has no instructions: the body is its system prompt"),
        );
    }
    Some(AgentFile {
        name,
        frontmatter: fm,
        body,
        remote,
    })
}

/// Warn when a prompt is longer than Copilot accepts, so a file that must stay portable does
/// not break there. Called by the loader with the whole prompt, parts included.
pub(crate) fn check_prompt_length(sink: &mut Sink<'_>, prompt: &str) {
    let chars = prompt.chars().count();
    if chars > COPILOT_PROMPT_CAP {
        sink.warn(
            None,
            format!(
                "the prompt has {chars} characters; GitHub Copilot accepts at most \
                 {COPILOT_PROMPT_CAP} in an agent file"
            ),
        );
    }
}

fn resolve_name(
    sink: &mut Sink<'_>,
    split: &Split<'_>,
    fm: &AgentFrontmatter,
    kind: FileKind,
    path_name: Option<&str>,
) -> Option<String> {
    let line = key_line(split, "name");
    if kind == FileKind::Named {
        let dir = path_name.unwrap_or_default();
        if !is_agent_name(dir) {
            sink.error(
                None,
                format!(
                    "the directory name `{dir}` is not a valid agent name \
                     (`a-z`, `0-9`, `-`, `_`; at most 64 characters, starting with a letter or digit)"
                ),
            );
            return None;
        }
        if let Some(n) = fm.name.as_deref().filter(|n| *n != dir) {
            sink.error(
                line,
                format!("`name: {n}` does not match the directory `{dir}`"),
            );
        }
        return Some(dir.to_owned());
    }

    let mut invalid_note = None;
    if let Some(n) = fm.name.as_deref() {
        if is_agent_name(n) {
            return Some(n.to_owned());
        }
        invalid_note = Some(n);
    }
    // The path's name: a Copilot file may be `Code-Reviewer.agent.md`; lower-casing it is the
    // one repair made, and it is announced.
    if let Some(p) = path_name.filter(|p| !p.is_empty()) {
        if is_agent_name(p) {
            if let Some(n) = invalid_note {
                sink.warn(line, invalid_name_message(n, p));
            }
            return Some(p.to_owned());
        }
        let lower = p.to_ascii_lowercase();
        if is_agent_name(&lower) {
            sink.warn(
                line,
                format!(
                    "`{}` is not a valid agent name (lower case letters, digits, `-`, `_`); using `{lower}`",
                    invalid_note.unwrap_or(p)
                ),
            );
            return Some(lower);
        }
    }
    match (invalid_note, path_name) {
        (Some(n), _) => sink.error(line, invalid_name_message(n, "")),
        (None, Some(p)) if !p.is_empty() => sink.error(
            None,
            format!("cannot derive an agent name from `{p}`: use lower case letters, digits, `-` and `_`"),
        ),
        _ => sink.error(
            None,
            "the agent has no name: set `name` in the frontmatter",
        ),
    }
    None
}

fn invalid_name_message(name: &str, using: &str) -> String {
    let tail = if using.is_empty() {
        String::new()
    } else {
        format!("; using `{using}`")
    };
    format!(
        "`name: {name}` is not a valid agent name (`a-z`, `0-9`, `-`, `_`; at most 64 characters, \
         starting with a letter or digit){tail}"
    )
}

/// `retry:`, the retry policy per tool (ADR 0061, decision 6): the shape and the bounds. Whether a
/// name is one of the agent's tools is for binding, which knows them.
fn check_retry(sink: &mut Sink<'_>, split: &Split<'_>, fm: &AgentFrontmatter) {
    if fm.retry.is_empty() {
        return;
    }
    let line = key_line(split, "retry");
    if fm.a2a.is_some() {
        sink.warn(
            line,
            "`retry` is ignored on a remote subagent: its tools are the remote agent's",
        );
        return;
    }
    for (tool, def) in &fm.retry {
        let at = |what: &str| format!("`retry.{tool}{what}`");
        if !is_retry_tool_name(tool) {
            sink.error(
                line,
                format!(
                    "{} is not a tool name (`A-Z`, `a-z`, `0-9`, `_`, `-`, at most 64 characters; \
                     `<server>__<tool>` for an MCP server's)",
                    at("")
                ),
            );
        }
        match def.attempts {
            None => sink.error(
                line,
                format!("{} needs `attempts`, the tries in all", at("")),
            ),
            Some(n) if !(1..=MAX_RETRY_ATTEMPTS).contains(&n) => sink.error(
                line,
                format!(
                    "{} is from 1 to {MAX_RETRY_ATTEMPTS}; it is {n}",
                    at(".attempts")
                ),
            ),
            Some(_) => {}
        }
        let duration = |text: &Option<String>, what: &str, sink: &mut Sink<'_>| {
            let text = text.as_deref()?;
            let parsed = retry_duration(text);
            if parsed.is_none() {
                sink.error(
                    line,
                    format!(
                        "{} is a duration such as `500ms`, `1s`, `30m` or `1h`; it is `{text}`",
                        at(what)
                    ),
                );
            }
            parsed
        };
        let backoff = duration(&def.backoff, ".backoff", sink);
        let bound = duration(&def.max_backoff, ".max_backoff", sink);
        for (value, what) in [(backoff, ".backoff"), (bound, ".max_backoff")] {
            if value.is_some_and(|d| d > MAX_RETRY_BACKOFF) {
                sink.error(line, format!("{} is at most 1h", at(what)));
            }
        }
        if let Some(bound) = bound
            && bound < backoff.unwrap_or(DEFAULT_RETRY_BACKOFF)
        {
            sink.error(
                line,
                format!(
                    "{} is shorter than {}",
                    at(".max_backoff"),
                    if def.backoff.is_some() {
                        "`backoff`"
                    } else {
                        "the default `backoff` (1s): give `backoff` too"
                    }
                ),
            );
        }
        if def.multiplier.is_some() && !def.factor().is_some_and(|m| m > 0.0) {
            sink.error(
                line,
                format!("{} is a positive number, such as 2.0", at(".multiplier")),
            );
        }
        if let Some(on) = &def.on {
            if on.is_empty() {
                sink.error(
                    line,
                    format!(
                        "{} names at least one class: {}",
                        at(".on"),
                        RETRY_CLASS_NAMES.join(", ")
                    ),
                );
            }
            for class in on
                .iter()
                .filter(|c| !RETRY_CLASS_NAMES.contains(&c.as_str()))
            {
                sink.error(
                    line,
                    format!(
                        "{} names `{class}`, which is not a retryable class; expected one of: {}",
                        at(".on"),
                        RETRY_CLASS_NAMES.join(", ")
                    ),
                );
            }
        }
        // `retry` is adam's own key: a misspelt part would leave the policy weaker than written.
        for key in def.extra.keys() {
            sink.error(
                line,
                format!(
                    "unknown key {}; expected one of: attempts, backoff, max_backoff, multiplier, on",
                    at(&format!(".{key}"))
                ),
            );
        }
    }
}

/// Secrets and endpoints (error), Claude and Copilot keys adam ignores (warning), the rest
/// (warning).
fn check_keys(sink: &mut Sink<'_>, split: &Split<'_>, fm: &AgentFrontmatter) {
    for key in fm.extra.keys() {
        let line = key_line(split, key);
        let normalized: String = key
            .chars()
            .filter(|c| *c != '_' && *c != '-')
            .flat_map(char::to_lowercase)
            .collect();
        if SECRET_KEYS.contains(&normalized.as_str()) {
            sink.error(
                line,
                format!("`{key}` is not allowed: secrets and endpoints belong in the environment"),
            );
        } else if key == "mcpServers" || key == "mcp-servers" {
            sink.warn(
                line,
                format!(
                    "`{key}` is ignored; put MCP servers in `mcp.json` next to the instructions"
                ),
            );
        } else if IGNORED_KEYS.contains(&key.as_str()) {
            sink.warn(
                line,
                format!("`{key}` is a Claude Code or Copilot key that adam ignores"),
            );
        } else {
            sink.warn(line, format!("unknown key `{key}` is ignored"));
        }
    }
    if let Some(limits) = &fm.limits {
        for key in limits.extra.keys() {
            sink.warn(
                key_line(split, "limits"),
                format!("unknown key `limits.{key}` is ignored"),
            );
        }
    }
    if let Some(card) = &fm.card {
        let line = key_line(split, "card");
        for key in card.extra.keys() {
            sink.warn(line, format!("unknown key `card.{key}` is ignored"));
        }
        for skill in &card.skills {
            for key in skill.extra.keys() {
                sink.warn(
                    line,
                    format!("unknown key `card.skills[{}].{key}` is ignored", skill.id),
                );
            }
        }
        if let Some(extended) = &card.extended {
            for key in extended.extra.keys() {
                sink.warn(
                    line,
                    format!("unknown key `card.extended.{key}` is ignored"),
                );
            }
            for skill in &extended.skills {
                for key in skill.extra.keys() {
                    sink.warn(
                        line,
                        format!(
                            "unknown key `card.extended.skills[{}].{key}` is ignored",
                            skill.id
                        ),
                    );
                }
            }
        }
    }
}

fn check_values(sink: &mut Sink<'_>, split: &Split<'_>, fm: &AgentFrontmatter, kind: FileKind) {
    if let Some(ModelRef::Alias(alias)) = &fm.model
        && alias.contains("://")
    {
        sink.error(
            key_line(split, "model"),
            "`model` is a gateway alias, not an endpoint: endpoints belong in the environment",
        );
    }
    if let Some(ToolList::Named(tools)) = &fm.tools {
        let unbindable: Vec<String> = tools
            .iter()
            .filter(|t| !is_tool_name(t) && !is_mcp_pattern(t))
            .map(|t| format!("`{t}`"))
            .collect();
        if !unbindable.is_empty() {
            let (verb, noun) = if unbindable.len() == 1 {
                ("is not an", "tool name")
            } else {
                ("are not", "tool names")
            };
            sink.warn(
                key_line(split, "tools"),
                format!(
                    "{} {verb} adam {noun} (`^[a-z][a-z0-9_]{{0,63}}$`, or `server__*` for MCP); \
                     binding will fail unless they are renamed or removed",
                    unbindable.join(", ")
                ),
            );
        }
    }
    if let Some(minutes) = fm.limits.as_ref().and_then(|limits| limits.max_minutes)
        && minutes > MAX_MINUTES_LIMIT
    {
        sink.error(
            key_line(split, "limits"),
            format!(
                "`limits.max_minutes` is at most {MAX_MINUTES_LIMIT} (24 hours), and `0` means no limit; it is {minutes}"
            ),
        );
    }
    if let Some(n) = fm.limits.as_ref().and_then(|limits| limits.max_parallel)
        && !(1..=MAX_PARALLEL_LIMIT).contains(&n)
    {
        sink.error(
            key_line(split, "limits"),
            format!(
                "`limits.max_parallel` is from 1 to {MAX_PARALLEL_LIMIT} (the tool calls of a turn that run at once); it is {n}"
            ),
        );
    }
    // The session limits are the root's: a subagent's are ignored (with a warning), so they are
    // not range-checked there.
    if let Some(limits) = fm.limits.as_ref().filter(|_| kind != FileKind::Sub) {
        let (low, high) = MAX_SESSION_ROUNDS_LIMIT;
        let ranges = [
            (
                "max_open_sessions",
                limits.max_open_sessions,
                1,
                MAX_OPEN_SESSIONS_LIMIT,
            ),
            (
                "max_working_sessions",
                limits.max_working_sessions,
                1,
                MAX_WORKING_SESSIONS_LIMIT,
            ),
            ("max_session_rounds", limits.max_session_rounds, low, high),
        ];
        for (key, value, low, high) in ranges {
            if let Some(n) = value
                && !(low..=high).contains(&n)
            {
                sink.error(
                    key_line(split, "limits"),
                    format!("`limits.{key}` is from {low} to {high}; it is {n}"),
                );
            }
        }
        if let (Some(open), Some(working)) = (limits.max_open_sessions, limits.max_working_sessions)
            && working > open
        {
            sink.error(
                key_line(split, "limits"),
                format!(
                    "`limits.max_working_sessions` is {working}, more than `limits.max_open_sessions` ({open})"
                ),
            );
        }
    }
    if let Err(why) = check_references(&fm.references) {
        sink.error(
            key_line(split, "references"),
            format!("`references`: {why}"),
        );
    }
    for key in fm.vars.keys() {
        if !is_env_name(key) {
            sink.error(
                key_line(split, "vars"),
                format!(
                    "var `{key}` cannot be a `{{{{placeholder}}}}`: use letters, digits and `_`"
                ),
            );
        }
    }
}

/// `server__*` or `server__tool` for an MCP server.
fn is_mcp_pattern(entry: &str) -> bool {
    let Some((server, tool)) = entry.split_once("__") else {
        return false;
    };
    let word = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    word(server) && (tool == "*" || word(tool))
}

/// The remote target of a subagent, when it has `a2a:`.
fn remote_spec(
    sink: &mut Sink<'_>,
    split: &Split<'_>,
    fm: &AgentFrontmatter,
) -> Option<RemoteSpec> {
    let Some(target) = fm.a2a.as_deref() else {
        if fm.auth.is_some() {
            sink.error(
                key_line(split, "auth"),
                "`auth` needs `a2a`: it authenticates to a remote subagent",
            );
        }
        return None;
    };
    let line = key_line(split, "a2a");
    let url = target.trim();
    match Url::parse(url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => {}
        _ => {
            sink.error(
                line,
                format!("`a2a: {url}` is not an http(s) agent-card URL"),
            );
            return None;
        }
    }
    let auth = match fm.auth.as_deref() {
        None => None,
        Some(a) => match a.trim().strip_prefix("bearer:") {
            Some(var) if is_env_name(var.trim()) => Some(RemoteAuth::Bearer {
                env: var.trim().to_owned(),
            }),
            _ => {
                sink.error(
                    key_line(split, "auth"),
                    "`auth` must be `bearer:ENV_VAR`, naming the environment variable that holds the token",
                );
                None
            }
        },
    };
    let local_only: Vec<&str> = [
        ("tools", fm.tools.is_some()),
        ("model", fm.model.is_some()),
        ("skills", fm.skills.is_some()),
        ("preload_skills", fm.preload_skills.is_some()),
        ("limits", fm.limits.is_some()),
        ("vars", !fm.vars.is_empty()),
    ]
    .into_iter()
    .filter_map(|(k, set)| set.then_some(k))
    .collect();
    for key in local_only {
        sink.warn(
            key_line(split, key),
            format!("`{key}` is ignored on a remote subagent (`a2a`)"),
        );
    }
    Some(RemoteSpec {
        url: url.to_owned(),
        auth,
        files: fm.files.unwrap_or(false),
    })
}
