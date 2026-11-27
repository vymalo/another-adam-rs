//! The tools a session's `config` narrows ([ADR 0054], slice 3): `tools`, an allow-list, and
//! `deny_tools`, a deny-list applied after it.
//!
//! **The loop applies it, not a source**, so every agent has it, an assembled one and one an embedder
//! builds with [`LlmAgentBuilder`](crate::LlmAgentBuilder) alike. [`SessionNarrowing`] is made at
//! each model turn and at each call from the run's state, and the agent consults it first, before
//! the sources' own holds, at both points ADR 0036 decision 6 names: the tools shown to the model
//! (a hidden tool is **not in the request**) and the call about to run (a call to one is **refused
//! without running**).
//!
//! **The truth is the session's header** (`Conversation::session`, its `config`): it is the state
//! of the run, committed with it. A round's context also carries the same lists under
//! [`SESSION_TOOLS_CONTEXT_KEY`], and the narrowing reads both, a tool being held back when either
//! says so; but the context is not the truth, because an oversized context is dropped, and a
//! narrowing must not go with it.
//!
//! **It only narrows**, like a mode ([`MODE_CONTEXT_KEY`](crate::MODE_CONTEXT_KEY)): the tier and
//! the mode still hold back what they hold back, and a list cannot offer a tool the agent does not
//! have. Entries are the folder's own syntax: exact names and `*` patterns (`github__*` selects an
//! MCP server). `read_output` is never held back: a round that cannot read a result it was handed
//! could not finish. A run that is no round of a configured session has no narrowing.
//!
//! [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md

use adam_model::ToolSpec;
use adam_runtime::{SessionConfig, glob};
use serde_json::{Map, Value};

use crate::conversation::{Conversation, SESSION_TOOLS_CONTEXT_KEY};
use crate::kept::READ_OUTPUT;
use crate::source::Withheld;

/// What a session's `tools` and `deny_tools` hold back of a run's tools.
///
/// Every allow-list must select a tool for it to be offered, and any entry of any deny-list holds it
/// back; a value of the wrong shape (only a hand-made context can have one) **fails closed**: an
/// allow-list that is not a list of names allows nothing, and a deny-list that is not one denies
/// everything. The default holds nothing back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionNarrowing {
    allows: Vec<Vec<String>>,
    denies: Vec<String>,
}

/// The names of a list written as JSON: `None` when the member is absent, `Err(())` when it is
/// there and **not** a list of strings (a mixed array is not one: reading its strings alone would
/// narrow less than was written).
fn names(value: Option<&Value>) -> Option<Result<Vec<String>, ()>> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::Array(entries)) if entries.iter().all(Value::is_string) => Some(Ok(entries
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect())),
        Some(_) => Some(Err(())),
    }
}

impl SessionNarrowing {
    /// The narrowing of a run: what its session header's `config` says (the truth) and what its
    /// context carries under [`SESSION_TOOLS_CONTEXT_KEY`] (a second, narrower-only reading).
    pub(crate) fn of(config: Option<&SessionConfig>, context: Option<&Map<String, Value>>) -> Self {
        let mut narrowing = Self::default();
        if let Some(config) = config {
            if let Some(tools) = &config.tools {
                narrowing.allows.push(tools.clone());
            }
            if let Some(deny) = &config.deny_tools {
                narrowing.denies.extend(deny.iter().cloned());
            }
        }
        if let Some(entry) = context.and_then(|context| context.get(SESSION_TOOLS_CONTEXT_KEY)) {
            let Some(entry) = entry.as_object() else {
                narrowing.allows.push(Vec::new());
                return narrowing;
            };
            match names(entry.get("tools")) {
                Some(Ok(allow)) => narrowing.allows.push(allow),
                Some(Err(())) => narrowing.allows.push(Vec::new()),
                None => {}
            }
            match names(entry.get("denyTools")) {
                Some(Ok(deny)) => narrowing.denies.extend(deny),
                Some(Err(())) => narrowing.denies.push("*".to_owned()),
                None => {}
            }
        }
        narrowing
    }

    /// Whether it holds nothing back.
    pub(crate) fn is_inert(&self) -> bool {
        self.allows.is_empty() && self.denies.is_empty()
    }

    /// Whether `tool` is held back. `read_output` never is.
    fn holds_back(&self, tool: &str) -> bool {
        if tool == READ_OUTPUT {
            return false;
        }
        let matched = |entries: &[String]| entries.iter().any(|entry| glob(entry, tool));
        self.allows.iter().any(|allow| !matched(allow)) || matched(&self.denies)
    }

    /// The one tool `name`, if it is held back, with the words its call answers.
    pub(crate) fn hold(&self, name: &str) -> Option<Withheld> {
        self.holds_back(name)
            .then(|| Withheld::new(name, refusal(name)))
    }

    /// Which of `tools` are held back.
    pub(crate) fn held(&self, tools: &[&str]) -> Vec<Withheld> {
        tools.iter().filter_map(|tool| self.hold(tool)).collect()
    }

    /// Take the held tools out of what the model is shown.
    pub(crate) fn retain(&self, specs: &mut Vec<ToolSpec>) {
        if !self.is_inert() {
            specs.retain(|spec| !self.holds_back(&spec.name));
        }
    }
}

/// What the run's session holds back of its tools: read from the **header** the state carries,
/// which is the truth, and from the context's copy of the same lists.
pub(crate) fn narrowing_of(
    state: &Conversation,
    context: Option<&Map<String, Value>>,
) -> SessionNarrowing {
    SessionNarrowing::of(
        state.session.as_ref().and_then(|h| h.config.as_ref()),
        context,
    )
}

/// What a withheld tool's call answers: a result for the model, not for the person.
fn refusal(tool: &str) -> String {
    format!(
        "`{tool}` is not offered to this session: the settings it was started with leave it out. \
         Do not look for another way to the same end. Say in plain words what you did not do and \
         that the session's settings do not offer `{tool}`; whoever started the session can start \
         another with different settings. Then do the rest of the work."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TOOLS: [&str; 5] = [
        "read_file",
        "write_file",
        "linear__list_issues",
        READ_OUTPUT,
        "subagent_start",
    ];

    fn config(tools: Option<&[&str]>, deny: Option<&[&str]>) -> SessionConfig {
        let own = |names: Option<&[&str]>| {
            names.map(|names| names.iter().map(|n| (*n).to_owned()).collect())
        };
        // `SessionConfig` is `#[non_exhaustive]`: another crate sets its fields one by one.
        #[allow(clippy::field_reassign_with_default)]
        let mut config = SessionConfig::default();
        config.tools = own(tools);
        config.deny_tools = own(deny);
        config
    }

    fn keyed(entry: Value) -> Map<String, Value> {
        Map::from_iter([(SESSION_TOOLS_CONTEXT_KEY.to_owned(), entry)])
    }

    fn held(narrowing: &SessionNarrowing) -> Vec<String> {
        narrowing.held(&TOOLS).into_iter().map(|h| h.tool).collect()
    }

    const ALL_BUT_READ_OUTPUT: [&str; 4] = [
        "read_file",
        "write_file",
        "linear__list_issues",
        "subagent_start",
    ];

    #[test]
    fn no_config_and_no_key_hold_nothing_back() {
        assert!(SessionNarrowing::of(None, None).is_inert());
        assert!(
            SessionNarrowing::of(Some(&config(None, None)), Some(&keyed(json!({})))).is_inert()
        );
        assert!(held(&SessionNarrowing::of(None, None)).is_empty());
    }

    #[test]
    fn an_allow_list_holds_back_what_it_does_not_match_but_read_output() {
        let n = SessionNarrowing::of(Some(&config(Some(&["read_file"]), None)), None);
        assert_eq!(
            held(&n),
            ["write_file", "linear__list_issues", "subagent_start"]
        );
        let n = SessionNarrowing::of(Some(&config(Some(&["read_*", "linear__*"]), None)), None);
        assert_eq!(held(&n), ["write_file", "subagent_start"]);
        // An empty allow-list offers nothing but `read_output`.
        let n = SessionNarrowing::of(Some(&config(Some(&[]), None)), None);
        assert_eq!(held(&n), ALL_BUT_READ_OUTPUT);
    }

    #[test]
    fn a_deny_list_wins_over_the_allow_list_and_never_takes_read_output() {
        let n = SessionNarrowing::of(
            Some(&config(
                Some(&["read_file", "write_file"]),
                Some(&["write_*"]),
            )),
            None,
        );
        assert_eq!(
            held(&n),
            ["write_file", "linear__list_issues", "subagent_start"]
        );
        let n = SessionNarrowing::of(Some(&config(None, Some(&["*"]))), None);
        assert_eq!(held(&n), ALL_BUT_READ_OUTPUT);
    }

    #[test]
    fn the_key_narrows_too_and_never_widens_what_the_header_says() {
        let header = config(Some(&["read_file", "write_file"]), None);
        let n = SessionNarrowing::of(
            Some(&header),
            Some(&keyed(
                json!({"tools": ["read_file"], "denyTools": ["subagent_*"]}),
            )),
        );
        assert_eq!(
            held(&n),
            ["write_file", "linear__list_issues", "subagent_start"]
        );
        // A key that allows more than the header does not offer more.
        let n = SessionNarrowing::of(Some(&header), Some(&keyed(json!({"tools": ["*"]}))));
        assert_eq!(held(&n), ["linear__list_issues", "subagent_start"]);
        // A key with no header narrows a run on its own.
        let n = SessionNarrowing::of(None, Some(&keyed(json!({"tools": ["read_file"]}))));
        assert_eq!(
            held(&n),
            ["write_file", "linear__list_issues", "subagent_start"]
        );
    }

    #[test]
    fn the_header_holds_when_the_context_lost_its_key() {
        let n = SessionNarrowing::of(
            Some(&config(Some(&["read_file"]), None)),
            Some(&Map::from_iter([("other".to_owned(), json!(1))])),
        );
        assert_eq!(
            held(&n),
            ["write_file", "linear__list_issues", "subagent_start"]
        );
    }

    #[test]
    fn a_value_of_the_wrong_shape_fails_closed() {
        for bad in [
            json!("read_file"),
            json!({"tools": "read_file"}),
            // A mixed list is not a list of names: its strings alone would narrow less than written.
            json!({"tools": ["read_file", 1]}),
        ] {
            let n = SessionNarrowing::of(None, Some(&keyed(bad.clone())));
            assert_eq!(held(&n), ALL_BUT_READ_OUTPUT, "{bad}");
        }
        for bad in [
            json!({"denyTools": "write_file"}),
            json!({"denyTools": ["write_file", 1]}),
            json!({"denyTools": [null]}),
        ] {
            let n = SessionNarrowing::of(None, Some(&keyed(bad.clone())));
            assert_eq!(held(&n), ALL_BUT_READ_OUTPUT, "{bad}");
        }
    }

    #[test]
    fn the_refusal_names_the_tool_and_tells_the_model_to_say_so() {
        let n = SessionNarrowing::of(Some(&config(Some(&["read_file"]), None)), None);
        let held = n.hold("write_file").expect("held");
        assert_eq!(held.tool, "write_file");
        assert!(held.reason.contains("`write_file` is not offered"));
        assert!(held.reason.contains("Do not look for another way"));
        assert!(held.reason.contains("Say in plain words"));
        assert!(n.hold("read_file").is_none());
        assert!(n.hold(READ_OUTPUT).is_none());
    }

    #[test]
    fn retain_takes_the_held_tools_out_of_the_specs() {
        let spec = |name: &str| ToolSpec {
            name: name.to_owned(),
            description: String::new(),
            parameters: json!({"type": "object"}),
        };
        let mut specs: Vec<ToolSpec> = TOOLS.iter().map(|name| spec(name)).collect();
        SessionNarrowing::default().retain(&mut specs);
        assert_eq!(specs.len(), 5);
        SessionNarrowing::of(Some(&config(Some(&["read_file"]), None)), None).retain(&mut specs);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["read_file", READ_OUTPUT]);
    }

    /// The invariant (ADR 0054 slice 3): a round whose header carries `tools` never offers or runs a
    /// tool they do not select, **even when its context lost the key**. A merge of a context over
    /// the limit drops the key (it is not protected), and the narrowing holds all the same.
    #[test]
    fn an_oversized_context_that_drops_the_key_leaves_the_header_narrowing_in_force() {
        let mut state = Conversation {
            session: Some(adam_runtime::SessionHeader {
                id: adam_core::RunId::new(),
                agent: "root/helper".into(),
                description: String::new(),
                keep_open: false,
                closed: false,
                closing: false,
                round: 1,
                root: None,
                workspace: None,
                visible: false,
                config: Some(config(Some(&["read_file"]), None)),
            }),
            ..Conversation::default()
        };
        let mut update = keyed(json!({"tools": ["read_file"]}));
        update.insert(
            "filler".to_owned(),
            json!("x".repeat(crate::MAX_CONTEXT_BYTES + 1)),
        );
        assert!(
            !state.merge_context(&update),
            "the context is over its limit"
        );
        assert!(
            !state.context.contains_key(SESSION_TOOLS_CONTEXT_KEY),
            "the unprotected key went with the rest"
        );
        let n = narrowing_of(&state, Some(&state.context));
        assert_eq!(
            held(&n),
            ["write_file", "linear__list_issues", "subagent_start"]
        );
        // Both points read it: the request's tools, and the call about to run.
        let mut specs: Vec<ToolSpec> = ["read_file", "write_file", READ_OUTPUT]
            .iter()
            .map(|name| ToolSpec {
                name: (*name).to_owned(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            })
            .collect();
        n.retain(&mut specs);
        assert_eq!(
            specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["read_file", READ_OUTPUT]
        );
        assert!(n.hold("write_file").is_some());
    }
}
