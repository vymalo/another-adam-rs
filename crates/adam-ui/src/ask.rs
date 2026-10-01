//! `ask_user { question, choices? }`: ask the person, with a form when there are fixed answers.
//!
//! With no `choices` it is the plain question every adam agent has: the run parks as
//! `input-required` with the question as text. With `choices` and a screen that has the `Choices`
//! component, the question carries a surface (one `Choices` of up to 8 questions, a radio list each,
//! checkboxes with `multiple`) and the person answers all of them with one action, which comes back
//! as this call's result (`- db: pg`). On a screen without the component, or when the catalog cannot
//! be read, the options are listed in the question's text and the answer is free text: the tool
//! degrades, it never fails the run.

use std::collections::HashSet;
use std::sync::Arc;

use adam_llm_agent::{Tool, ToolCtx, ToolError, ToolOutput, parse_args};
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::resolve::{Resolved, UiState};
use crate::surface::{safe_id, surface};

/// The tool's name.
pub const ASK_USER: &str = "ask_user";

/// The most questions one `choices` may hold: the `Choices` component's limit.
const MAX_QUESTIONS: usize = 8;
/// The most options a question may hold, and the fewest.
const MAX_OPTIONS: usize = 8;
const MIN_OPTIONS: usize = 2;
/// The longest option value: the component's pattern says 64.
const MAX_VALUE: usize = 64;
/// The name of the action a Choices sends.
const ANSWER_ACTION: &str = "answer";

/// What the description of `ask_user` opens with unless the agent says otherwise
/// ([`Ui::with_ask_lead`](crate::Ui::with_ask_lead)): when to ask.
pub const DEFAULT_ASK_LEAD: &str = "Ask the person you are talking to a question and wait for the answer. Use it only when you cannot proceed without it, or to get explicit consent before something that cannot be undone. Be specific.";

/// What the description of `ask_user` says after the lead, about `choices`.
const CHOICES_GUIDE: &str = "To ask several questions with fixed answers at once, pass `choices`: the person gets one form with a list of options per question and answers them together; their answers come back as the result. Without `choices` the question is plain text.";

/// What the model passes.
#[derive(Debug, Deserialize)]
struct Args {
    question: String,
    #[serde(default)]
    choices: Vec<ChoiceArg>,
}

/// One question with fixed answers.
#[derive(Debug, Deserialize)]
struct ChoiceArg {
    id: Option<String>,
    question: String,
    options: Vec<OptionArg>,
    multiple: Option<bool>,
    #[serde(rename = "allowOther", alias = "allow_other")]
    allow_other: Option<bool>,
    required: Option<bool>,
}

/// An option: a label (its value is made from it) or an object.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OptionArg {
    Label(String),
    Full {
        value: Option<String>,
        label: String,
        description: Option<String>,
    },
}

/// A question as the component takes it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Question {
    id: String,
    question: String,
    options: Vec<ChoiceOption>,
    multiple: Option<bool>,
    allow_other: Option<bool>,
    required: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChoiceOption {
    value: String,
    label: String,
    description: Option<String>,
}

/// `label` as an option value: lowercase ASCII letters and digits, any other run of characters
/// one `-`, no `-` at either end, at most 64 characters. Nothing left: `o<position>`.
fn slug(label: &str, position: usize) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    out.truncate(MAX_VALUE);
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() {
        format!("o{position}")
    } else {
        out
    }
}

/// `base` made unique among `taken` by a `-2`, `-3`, ... suffix, within 64 characters.
fn unique(base: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| {
            let suffix = format!("-{n}");
            let room = MAX_VALUE - suffix.len();
            let head: String = base.chars().take(room).collect();
            format!("{head}{suffix}")
        })
        .find(|candidate| !taken.contains(candidate))
        .unwrap_or_else(|| base.to_owned())
}

/// The questions of a call, with every id and value made and checked.
///
/// # Errors
///
/// What the model got wrong, in words it can act on.
fn normalize(choices: Vec<ChoiceArg>) -> Result<Vec<Question>, String> {
    if choices.len() > MAX_QUESTIONS {
        return Err(format!(
            "`choices` has {} questions; at most {MAX_QUESTIONS} fit in one form: ask the rest afterwards",
            choices.len()
        ));
    }
    let mut ids: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(choices.len());
    for (n, choice) in choices.into_iter().enumerate() {
        let n = n + 1;
        let question = choice.question.trim().to_owned();
        if question.is_empty() {
            return Err(format!("question {n} of `choices` has no text"));
        }
        let id = match choice
            .id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(id) => id.to_owned(),
            None => unique(&format!("q{n}"), &ids),
        };
        if !ids.insert(id.clone()) {
            return Err(format!("the question id `{id}` is used twice in `choices`"));
        }
        if choice.options.len() < MIN_OPTIONS || choice.options.len() > MAX_OPTIONS {
            return Err(format!(
                "question `{id}` has {} options; it needs {MIN_OPTIONS} to {MAX_OPTIONS}",
                choice.options.len()
            ));
        }
        let mut values: HashSet<String> = HashSet::new();
        let mut options = Vec::with_capacity(choice.options.len());
        for (m, option) in choice.options.into_iter().enumerate() {
            let m = m + 1;
            let (explicit, label, description) = match option {
                OptionArg::Label(label) => (None, label, None),
                OptionArg::Full {
                    value,
                    label,
                    description,
                } => (value, label, description),
            };
            let label = label.trim().to_owned();
            if label.is_empty() {
                return Err(format!("option {m} of question `{id}` has no label"));
            }
            let value = match explicit.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(value) => {
                    if !values.insert(value.to_owned()) {
                        return Err(format!(
                            "the option value `{value}` is used twice in question `{id}`"
                        ));
                    }
                    value.to_owned()
                }
                None => {
                    let value = unique(&slug(&label, m), &values);
                    values.insert(value.clone());
                    value
                }
            };
            options.push(ChoiceOption {
                value,
                label,
                description: description
                    .map(|d| d.trim().to_owned())
                    .filter(|d| !d.is_empty()),
            });
        }
        out.push(Question {
            id,
            question,
            options,
            multiple: choice.multiple,
            allow_other: choice.allow_other,
            required: choice.required,
        });
    }
    Ok(out)
}

/// The `Choices` component instance for `questions`: the root of the surface.
fn choices_instance(questions: &[Question]) -> Value {
    let questions: Vec<Value> = questions
        .iter()
        .map(|q| {
            let options: Vec<Value> = q
                .options
                .iter()
                .map(|o| {
                    let mut option = Map::new();
                    option.insert("value".into(), json!(o.value));
                    option.insert("label".into(), json!(o.label));
                    if let Some(description) = &o.description {
                        option.insert("description".into(), json!(description));
                    }
                    Value::Object(option)
                })
                .collect();
            let mut question = Map::new();
            question.insert("id".into(), json!(q.id));
            question.insert("question".into(), json!(q.question));
            question.insert("options".into(), Value::Array(options));
            if let Some(multiple) = q.multiple {
                question.insert("multiple".into(), json!(multiple));
            }
            if let Some(other) = q.allow_other {
                question.insert("allowOther".into(), json!(other));
            }
            if let Some(required) = q.required {
                question.insert("required".into(), json!(required));
            }
            Value::Object(question)
        })
        .collect();
    json!({
        "id": "root",
        "component": "Choices",
        "questions": questions,
        "action": {"event": {"name": ANSWER_ACTION}},
    })
}

/// `question` followed by the questions and their options as text, for a screen that cannot draw
/// them: the person answers in words.
fn as_text(question: &str, questions: &[Question]) -> String {
    let mut text = question.to_owned();
    text.push('\n');
    for (n, q) in questions.iter().enumerate() {
        text.push_str(&format!("\n{}. {}", n + 1, q.question));
        if q.multiple == Some(true) {
            text.push_str(" (any number of these)");
        }
        for (m, option) in q.options.iter().enumerate() {
            let letter = char::from(b'a' + u8::try_from(m).unwrap_or(0));
            text.push_str(&format!("\n   {letter}) {}", option.label));
            if let Some(description) = &option.description {
                text.push_str(&format!(" - {description}"));
            }
        }
        if q.allow_other == Some(true) {
            text.push_str("\n   or say something else");
        }
    }
    text
}

/// `ask_user`. Made by [`Ui::tools`](crate::Ui::tools).
#[derive(Clone)]
pub struct AskUser {
    state: Arc<UiState>,
    lead: String,
}

impl std::fmt::Debug for AskUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskUser").finish_non_exhaustive()
    }
}

impl AskUser {
    pub(crate) fn new(state: Arc<UiState>, lead: impl Into<String>) -> Self {
        Self {
            state,
            lead: lead.into(),
        }
    }
}

#[async_trait]
impl Tool for AskUser {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ASK_USER.to_owned(),
            description: format!("{} {CHOICES_GUIDE}", self.lead.trim()),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "description": "What you need to know"},
                    "choices": {
                        "type": "array",
                        "maxItems": MAX_QUESTIONS,
                        "description": "Questions with fixed answers, asked together as one form (at most 8). Omit for an open question.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string", "description": "A short id for the question (letters, digits, - _ . :), used in the answer; defaults to q1, q2, ..."},
                                "question": {"type": "string", "description": "The question"},
                                "options": {
                                    "type": "array",
                                    "minItems": MIN_OPTIONS,
                                    "maxItems": MAX_OPTIONS,
                                    "description": "The answers to pick from: 2 to 8, each a label or {value, label, description}",
                                    "items": {"anyOf": [
                                        {"type": "string"},
                                        {"type": "object", "properties": {
                                            "value": {"type": "string", "description": "A short id for the option (letters, digits, - _ . :); made from the label when omitted"},
                                            "label": {"type": "string"},
                                            "description": {"type": "string"}},
                                         "required": ["label"]}]}
                                },
                                "multiple": {"type": "boolean", "description": "Several options may be picked"},
                                "allowOther": {"type": "boolean", "description": "Offer an \"Other\" the person can write"},
                                "required": {"type": "boolean", "description": "The person must answer this one (the default)"}
                            },
                            "required": ["question", "options"]
                        }
                    }
                },
                "required": ["question"]
            }),
        }
    }

    fn asks_user(&self) -> bool {
        true
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let args: Args = match parse_args(ASK_USER, args) {
            Ok(args) => args,
            Err(refusal) => return Ok(refusal),
        };
        let question = args.question.trim();
        if question.is_empty() {
            return Ok(ToolOutput::error("question is required"));
        }
        if args.choices.is_empty() {
            return Err(ToolError::needs_input(question));
        }
        let questions = match normalize(args.choices) {
            Ok(questions) => questions,
            Err(problem) => return Ok(ToolOutput::error(problem)),
        };
        match self.state.resolve(ctx.context_map()).await {
            Resolved::Found(catalog) if catalog.component("Choices").is_some() => {
                let instance = choices_instance(&questions);
                if let Err(problem) = catalog.validate(&instance) {
                    return Ok(ToolOutput::error(format!(
                        "the choices do not fit the screen's Choices component: {problem}"
                    )));
                }
                let id = format!("ask-{}", safe_id(ctx.call_id()));
                let ui = surface(&id, catalog.catalog_id(), vec![instance]);
                Err(ToolError::needs_input_with_ui(question, ui))
            }
            Resolved::Found(_) => {
                tracing::debug!("the screen has no Choices component; the options go in the text");
                Err(ToolError::needs_input(as_text(question, &questions)))
            }
            Resolved::NoCatalog => Err(ToolError::needs_input(as_text(question, &questions))),
            Resolved::Unreadable(reason) => {
                tracing::debug!(%reason, "the screen's components could not be read; the options go in the text");
                Err(ToolError::needs_input(as_text(question, &questions)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(value: Value) -> ChoiceArg {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_label_becomes_a_slug_and_nothing_left_becomes_the_position() {
        assert_eq!(slug("Postgres", 1), "postgres");
        assert_eq!(slug("Docker Compose!", 1), "docker-compose");
        assert_eq!(slug("  --a__b--  ", 1), "a-b");
        assert_eq!(slug("Kubernetes (k8s)", 1), "kubernetes-k8s");
        assert_eq!(slug("日本語", 3), "o3");
        assert_eq!(slug("!!!", 2), "o2");
        assert_eq!(slug(&"a".repeat(100), 1).len(), 64);
        // A cut that lands on a dash does not leave it.
        assert_eq!(slug(&format!("{} b", "a".repeat(63)), 1), "a".repeat(63));
    }

    #[test]
    fn a_taken_value_gets_a_numeric_suffix_within_the_limit() {
        let taken: HashSet<String> = ["a".to_owned(), "a-2".to_owned()].into();
        assert_eq!(unique("b", &taken), "b");
        assert_eq!(unique("a", &taken), "a-3");
        let long = "x".repeat(64);
        let taken: HashSet<String> = [long.clone()].into();
        let next = unique(&long, &taken);
        assert_eq!(next.len(), 64);
        assert!(next.ends_with("-2"));
    }

    #[test]
    fn options_given_as_labels_or_objects_get_ids_values_and_defaults() {
        let questions = normalize(vec![
            choice(json!({"question": "Which database?", "options": ["Postgres", "SQLite", "Postgres"]})),
            choice(json!({"id": "auth", "question": " Login? ", "multiple": true, "allowOther": true,
                "options": [{"value": "kc", "label": "Keycloak", "description": "SSO"}, {"label": "None"}]})),
        ])
        .unwrap();
        assert_eq!(questions[0].id, "q1");
        assert_eq!(
            questions[0]
                .options
                .iter()
                .map(|o| o.value.as_str())
                .collect::<Vec<_>>(),
            ["postgres", "sqlite", "postgres-2"],
            "a repeated label is told apart, not refused"
        );
        assert_eq!(questions[1].id, "auth");
        assert_eq!(questions[1].question, "Login?");
        assert_eq!(questions[1].options[0].value, "kc");
        assert_eq!(questions[1].options[0].description.as_deref(), Some("SSO"));
        assert_eq!(questions[1].options[1].value, "none");
        assert_eq!(questions[1].multiple, Some(true));
        assert_eq!(questions[1].required, None);
    }

    #[test]
    fn the_models_mistakes_come_back_in_words_it_can_act_on() {
        let one = |v: Value| normalize(vec![choice(v)]).unwrap_err();
        assert!(one(json!({"question": "q", "options": ["only"]})).contains("needs 2 to 8"));
        assert!(
            one(json!({"question": "q", "options": (0..9).map(|n| n.to_string()).collect::<Vec<_>>()}))
                .contains("needs 2 to 8")
        );
        assert!(one(json!({"question": " ", "options": ["a", "b"]})).contains("has no text"));
        assert!(one(json!({"question": "q", "options": ["a", " "]})).contains("has no label"));
        assert!(
            one(json!({"question": "q", "options": [{"value": "x", "label": "A"}, {"value": "x", "label": "B"}]}))
                .contains("`x` is used twice")
        );
        let twice = normalize(vec![
            choice(json!({"id": "d", "question": "q", "options": ["a", "b"]})),
            choice(json!({"id": "d", "question": "r", "options": ["a", "b"]})),
        ])
        .unwrap_err();
        assert!(twice.contains("`d` is used twice"), "{twice}");
        let many: Vec<ChoiceArg> = (0..9)
            .map(|n| choice(json!({"question": format!("q{n}"), "options": ["a", "b"]})))
            .collect();
        assert!(normalize(many).unwrap_err().contains("at most 8"));
    }

    #[test]
    fn the_instance_is_a_choices_component_with_an_answer_action() {
        let questions = normalize(vec![choice(json!({
            "id": "db", "question": "Which database?", "allowOther": true, "required": false,
            "options": [{"value": "pg", "label": "Postgres", "description": "default"}, "SQLite"]}))])
        .unwrap();
        assert_eq!(
            choices_instance(&questions),
            json!({
                "id": "root", "component": "Choices",
                "questions": [{"id": "db", "question": "Which database?",
                    "options": [{"value": "pg", "label": "Postgres", "description": "default"},
                                {"value": "sqlite", "label": "SQLite"}],
                    "allowOther": true, "required": false}],
                "action": {"event": {"name": "answer"}}
            })
        );
    }

    #[test]
    fn the_text_form_lists_the_questions_and_their_options() {
        let questions = normalize(vec![
            choice(json!({"question": "Which database?", "options": ["Postgres", {"label": "SQLite", "description": "embedded"}]})),
            choice(json!({"question": "Where?", "multiple": true, "allowOther": true, "options": ["k8s", "compose"]})),
        ])
        .unwrap();
        assert_eq!(
            as_text("Three quick questions", &questions),
            "Three quick questions\n\
             \n1. Which database?\n   a) Postgres\n   b) SQLite - embedded\
             \n2. Where? (any number of these)\n   a) k8s\n   b) compose\n   or say something else"
        );
    }

    #[test]
    fn the_spec_is_what_the_model_reads() {
        let state = Arc::new(UiState::new(Arc::new(
            crate::thread_tools::ThreadToolsClient::new(adam_mcp::McpPolicy::default()),
        )));
        let spec = AskUser::new(Arc::clone(&state), DEFAULT_ASK_LEAD).spec();
        assert_eq!(spec.name, "ask_user");
        assert!(
            spec.description
                .starts_with("Ask the person you are talking to a question")
        );
        assert!(
            spec.description
                .ends_with("Without `choices` the question is plain text.")
        );
        // An agent can open the description with its own words.
        let coder = AskUser::new(
            state,
            "Ask the person who gave you the task a question. Be specific.",
        )
        .spec();
        assert!(coder.description.starts_with(
            "Ask the person who gave you the task a question. Be specific. To ask several"
        ));
        assert_eq!(spec.parameters["required"], json!(["question"]));
        assert_eq!(spec.parameters["properties"]["choices"]["maxItems"], 8);
        let option = &spec.parameters["properties"]["choices"]["items"]["properties"]["options"];
        assert_eq!(option["minItems"], 2);
        assert!(option["items"]["anyOf"].is_array());
    }
}
