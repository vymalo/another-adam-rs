//! `ask_user { question, title?, description?, choices?, freeInput?, submitLabel? }`: ask the person,
//! with a form when there are fixed answers.
//!
//! With no `choices` it is the plain question every adam agent has: the run parks as
//! `input-required` with the question as text. With `choices` and a screen that has the `Choices`
//! component, the question carries a surface: one form with a `title`, a `description` of why the
//! agent asks, up to 8 questions (radio buttons, checkboxes, a searchable list, a deck of pictures,
//! or a typed answer; each option with a label, a description, an icon and a recommended mark) and an
//! optional "anything else" box. The person answers all of it with one action, which comes back as
//! this call's result (`- db: pg`). On a screen without the component, or when the catalog cannot be
//! read, the options are listed in the question's text and the answer is free text: the tool
//! degrades, it never fails the run.
//!
//! The screen's catalog says what it can draw: from catalog **version 5** the form is sent whole;
//! on version 2 to 4 it is sent as the subset that screen has (no types beyond radio buttons and
//! checkboxes, at most 8 options, the description folded into the question's text) and, when a
//! question needs more than that (a typed answer), as text.

use std::collections::HashSet;
use std::sync::Arc;

use adam_llm_agent::{StepIcon, StepStyle, Tool, ToolCtx, ToolError, ToolOutput, parse_args};
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::resolve::{Resolved, UiState};
use crate::surface::{ask_surface_id, surface};
use crate::words::is_format_or_control;

/// The tool's name.
pub const ASK_USER: &str = "ask_user";

/// The most questions one `choices` may hold: the `Choices` component's limit.
const MAX_QUESTIONS: usize = 8;
/// The fewest options a question that offers any has.
const MIN_OPTIONS: usize = 2;
/// The most options of a radio or checkbox question, of a list and of a deck.
const MAX_OPTIONS: usize = 12;
const MAX_SELECT_OPTIONS: usize = 50;
const MAX_DECK_IMAGES: usize = 16;
/// The most options a screen of catalog version 2 to 4 takes in a question.
const V4_MAX_OPTIONS: usize = 8;
/// The most characters of a typed answer's default: what the answer may be.
const MAX_TEXT: usize = 500;
const MAX_LONG_TEXT: usize = 2000;
/// The longest label of an option on a screen that has no "recommended" mark of its own.
const MAX_LABEL: usize = 120;
/// The longest question: the component's limit.
const MAX_QUESTION: usize = 300;
/// The longest option value: the component's pattern says 64.
const MAX_VALUE: usize = 64;
/// The first catalog version whose `Choices` has the title, description, types, pictures and the
/// free box (`docs/api/ui-catalog-v1.md` of `vymalo/another-agentic-system`, "Version 5").
const CATALOG_V5: u32 = 5;
/// The name of the action a Choices sends.
const ANSWER_ACTION: &str = "answer";

/// What the description of `ask_user` opens with unless the agent says otherwise
/// ([`Ui::with_ask_lead`](crate::Ui::with_ask_lead)): when to ask.
pub const DEFAULT_ASK_LEAD: &str = "Ask the person you are talking to a question and wait for the answer. Use it only when you cannot proceed without it, or to get explicit consent before something that cannot be undone. Be specific.";

/// What the description of `ask_user` says after the lead, about `choices`: the constraints the
/// tool enforces, so that the first call is right.
const CHOICES_GUIDE: &str = "`question` is always required, with or without `choices`: it is the sentence the person reads first. Whenever the answer is one of a few options, pass `choices` and never write the options as a list in your text: the person gets one form with a list of options per question and answers them together; their answers come back as the result, so \"yes\" is never ambiguous. Give the form a short `title` and a `description` of why you ask (a sentence or two of plain text). Each entry of `choices` needs its own `question`; give every option a short `label` and a `description` that says what it means or what it costs, and mark the one you recommend with `recommended: true`. A question's `type` is `single` (radio buttons, the default; 2 to 12 options), `multiple` (checkboxes; 2 to 12; `min` and `max` say how many), `select` (a searchable list; 2 to 50 options), `images` (a deck of pictures; 2 to 16 options, each with `image` {sha256, alt}: the sha256 of an image file you shared in this conversation, never a URL) or `text` / `longText` (the person types the answer; no options). `help` is a line under the question, `default` is what is chosen at first, `allowOther` offers an \"Other\" the person can write, `freeInput` adds an \"anything else\" box. With a single possible answer, or none that you know, do not use `choices`: ask a plain question. There are at most 8 questions. An `id` or option `value` is 1 to 64 characters of letters, digits and `- _ . :`. Example: {\"question\": \"A few things I need before I start.\", \"title\": \"Before I start\", \"description\": \"The migration depends on the database, so I need to know which one.\", \"choices\": [{\"id\": \"db\", \"question\": \"Which database?\", \"options\": [{\"label\": \"Postgres\", \"description\": \"Relational, the default\", \"recommended\": true}, {\"label\": \"MongoDB\", \"description\": \"Documents, no schema\"}]}]} Without `choices` the question is plain text.";

/// The pattern of a question id and of an option value: the `Choices` component's own.
const ID_PATTERN: &str = "^[A-Za-z0-9_.:-]{1,64}$";

/// The icons an option may have: the ids of `steps/v1`.
const ICONS: [&str; 15] = [
    "agent", "read", "edit", "delete", "move", "search", "execute", "think", "fetch", "web", "git",
    "test", "file", "tool", "opencode",
];

/// What the model passes.
#[derive(Debug, Deserialize)]
struct Args {
    question: String,
    title: Option<String>,
    description: Option<String>,
    #[serde(default)]
    choices: Vec<ChoiceArg>,
    #[serde(rename = "freeInput", alias = "free_input")]
    free_input: Option<FreeInputArg>,
    #[serde(rename = "submitLabel", alias = "submit_label")]
    submit_label: Option<String>,
}

/// The "anything else" box: `true`, or its words.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum FreeInputArg {
    Flag(bool),
    Full {
        label: Option<String>,
        placeholder: Option<String>,
    },
}

/// How a question is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Kind {
    Single,
    Multiple,
    Select,
    Images,
    Text,
    LongText,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Multiple => "multiple",
            Self::Select => "select",
            Self::Images => "images",
            Self::Text => "text",
            Self::LongText => "longText",
        }
    }

    fn is_typed(self) -> bool {
        matches!(self, Self::Text | Self::LongText)
    }
}

/// A default: one value, or several.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DefaultArg {
    One(String),
    Many(Vec<String>),
}

/// One question.
#[derive(Debug, Deserialize)]
struct ChoiceArg {
    id: Option<String>,
    question: String,
    help: Option<String>,
    #[serde(rename = "type")]
    kind: Option<Kind>,
    #[serde(default)]
    options: Vec<OptionArg>,
    multiple: Option<bool>,
    #[serde(rename = "allowOther", alias = "allow_other")]
    allow_other: Option<bool>,
    required: Option<bool>,
    default: Option<DefaultArg>,
    min: Option<u32>,
    max: Option<u32>,
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
        image: Option<ImageArg>,
        icon: Option<String>,
        recommended: Option<bool>,
    },
}

/// The picture of an option: a file of this conversation, by its hash.
#[derive(Debug, Deserialize)]
struct ImageArg {
    sha256: String,
    alt: String,
}

/// A form as the component takes it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Form {
    title: Option<String>,
    description: Option<String>,
    free_input: Option<FreeInput>,
    submit_label: Option<String>,
    questions: Vec<Question>,
}

#[derive(Debug, Clone, PartialEq)]
struct FreeInput {
    label: Option<String>,
    placeholder: Option<String>,
}

/// A question as the component takes it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Question {
    id: String,
    question: String,
    help: Option<String>,
    /// What the model said; `None` is the old spelling (`multiple` decides).
    declared: Option<Kind>,
    options: Vec<ChoiceOption>,
    multiple: Option<bool>,
    allow_other: Option<bool>,
    required: Option<bool>,
    /// The values chosen at first.
    defaults: Vec<String>,
    /// The words a typed question starts with.
    default_text: Option<String>,
    min: Option<u32>,
    max: Option<u32>,
}

impl Question {
    /// How the question is answered: what the model said, else radio buttons or checkboxes.
    fn kind(&self) -> Kind {
        self.declared.unwrap_or(if self.multiple == Some(true) {
            Kind::Multiple
        } else {
            Kind::Single
        })
    }

    /// Whether several options may be chosen.
    fn takes_several(&self) -> bool {
        match self.kind() {
            Kind::Multiple => true,
            Kind::Select | Kind::Images => self.multiple == Some(true),
            Kind::Single | Kind::Text | Kind::LongText => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChoiceOption {
    value: String,
    label: String,
    description: Option<String>,
    image: Option<(String, String)>,
    icon: Option<String>,
    recommended: bool,
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

/// `text` trimmed; `None` when nothing is left.
fn trimmed(text: Option<String>) -> Option<String> {
    text.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
}

/// The options of question `id` as the component takes them, with every value made and checked.
fn normalize_options(id: &str, options: Vec<OptionArg>) -> Result<Vec<ChoiceOption>, String> {
    let mut values: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(options.len());
    for (m, option) in options.into_iter().enumerate() {
        let m = m + 1;
        let (explicit, label, description, image, icon, recommended) = match option {
            OptionArg::Label(label) => (None, label, None, None, None, None),
            OptionArg::Full {
                value,
                label,
                description,
                image,
                icon,
                recommended,
            } => (value, label, description, image, icon, recommended),
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
        let image = match image {
            None => None,
            Some(ImageArg { sha256, alt }) => {
                let hash = sha256.trim().to_ascii_lowercase();
                let valid = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
                if !valid || alt.trim().is_empty() {
                    return Err(format!(
                        "the image of option `{value}` of question `{id}` needs a `sha256` (64 hexadecimal \
                         digits: the hash of an image file you shared in this conversation, never a URL) and an `alt`"
                    ));
                }
                Some((hash, alt.trim().to_owned()))
            }
        };
        out.push(ChoiceOption {
            value,
            label,
            description: trimmed(description),
            image,
            icon: trimmed(icon),
            recommended: recommended == Some(true),
        });
    }
    Ok(out)
}

/// What a `default` names, as option values: a value, or else a label (any case).
fn resolve_defaults(
    id: &str,
    default: DefaultArg,
    options: &[ChoiceOption],
) -> Result<Vec<String>, String> {
    let named = match default {
        DefaultArg::One(one) => vec![one],
        DefaultArg::Many(many) => many,
    };
    named
        .into_iter()
        .map(|name| {
            let name = name.trim();
            options
                .iter()
                .find(|o| o.value == name)
                .or_else(|| options.iter().find(|o| o.label.eq_ignore_ascii_case(name)))
                .map(|o| o.value.clone())
                .ok_or_else(|| {
                    format!("the default `{name}` of question `{id}` is not one of its options")
                })
        })
        .collect()
}

/// One question as the component takes it, or what the model got wrong.
fn normalize_question(
    n: usize,
    ids: &mut HashSet<String>,
    choice: ChoiceArg,
) -> Result<Question, String> {
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
        None => unique(&format!("q{n}"), ids),
    };
    if !ids.insert(id.clone()) {
        return Err(format!("the question id `{id}` is used twice in `choices`"));
    }
    let declared = choice.kind;
    let multiple = choice.multiple;
    if declared == Some(Kind::Single) && multiple == Some(true) {
        return Err(format!(
            "question `{id}` is type \"single\" but has `multiple: true`"
        ));
    }
    if declared == Some(Kind::Multiple) && multiple == Some(false) {
        return Err(format!(
            "question `{id}` is type \"multiple\" but has `multiple: false`"
        ));
    }
    let options = normalize_options(&id, choice.options)?;
    let mut out = Question {
        id,
        question,
        help: trimmed(choice.help),
        declared,
        options,
        multiple,
        allow_other: choice.allow_other,
        required: choice.required,
        defaults: Vec::new(),
        default_text: None,
        min: choice.min,
        max: choice.max,
    };
    let id = out.id.clone();
    let kind = out.kind();
    if kind.is_typed() {
        let limit = if kind == Kind::Text {
            MAX_TEXT
        } else {
            MAX_LONG_TEXT
        };
        if !out.options.is_empty() {
            return Err(format!(
                "question `{id}` is type \"{}\": a typed answer takes no options",
                kind.as_str()
            ));
        }
        if multiple == Some(true)
            || out.allow_other == Some(true)
            || out.min.is_some()
            || out.max.is_some()
        {
            return Err(format!(
                "question `{id}` is type \"{}\": it takes no `multiple`, `allowOther`, `min` or `max`",
                kind.as_str()
            ));
        }
        if let Some(default) = choice.default {
            let DefaultArg::One(text) = default else {
                return Err(format!(
                    "the default of question `{id}` is words, not a list"
                ));
            };
            let text = text.trim().to_owned();
            if let Some(hidden) = text
                .chars()
                .find(|c| !matches!(c, '\t' | '\n' | '\r') && is_format_or_control(*c))
            {
                return Err(format!(
                    "the default of question `{id}` has a character the person cannot see (U+{:04X}): \
                     a default is words they can read",
                    u32::from(hidden)
                ));
            }
            if text.chars().count() > limit {
                return Err(format!(
                    "the default of question `{id}` is over {limit} characters, the most a \"{}\" answer has",
                    kind.as_str()
                ));
            }
            out.default_text = Some(text).filter(|t| !t.is_empty());
        }
        return Ok(out);
    }

    let (most, name) = match kind {
        Kind::Select => (MAX_SELECT_OPTIONS, "a \"select\" question"),
        Kind::Images => (MAX_DECK_IMAGES, "an \"images\" question"),
        _ => (MAX_OPTIONS, "a radio or checkbox question"),
    };
    if out.options.len() < MIN_OPTIONS || out.options.len() > most {
        let advice = match kind {
            Kind::Select | Kind::Images => "",
            _ if out.options.len() > most => " (use type \"select\" for up to 50)",
            _ => "",
        };
        return Err(format!(
            "question `{id}` has {} options; {name} needs {MIN_OPTIONS} to {most}{advice} (with a single \
             possible answer, ask a plain question without `choices`)",
            out.options.len()
        ));
    }
    for option in &out.options {
        match (kind == Kind::Images, option.image.is_some()) {
            (true, false) => {
                return Err(format!(
                    "option `{}` of question `{id}` has no image: every option of an \"images\" question needs one",
                    option.value
                ));
            }
            (false, true) => {
                return Err(format!(
                    "option `{}` of question `{id}` has an image, which only type \"images\" draws",
                    option.value
                ));
            }
            _ => {}
        }
    }
    if let Some(default) = choice.default {
        out.defaults = resolve_defaults(&id, default, &out.options)?;
    }
    let several = out.takes_several();
    if !several && out.defaults.len() > 1 {
        return Err(format!(
            "question `{id}` takes one choice, but has {} defaults",
            out.defaults.len()
        ));
    }
    if !several {
        if out.min.is_some() || out.max.is_some() {
            return Err(format!(
                "question `{id}` takes one choice: `min` and `max` apply when several may be chosen (type \"multiple\")"
            ));
        }
        return Ok(out);
    }
    let offered = out.options.len() + usize::from(out.allow_other == Some(true));
    let as_count = |n: u32| usize::try_from(n).unwrap_or(usize::MAX);
    if let Some(max) = out.max
        && (max < 1 || as_count(max) > offered)
    {
        return Err(format!(
            "the `max` of question `{id}` is {max}, but it offers {offered} choices"
        ));
    }
    if let Some(min) = out.min
        && as_count(min) > offered
    {
        return Err(format!(
            "the `min` of question `{id}` is {min}, but it offers {offered} choices"
        ));
    }
    if let (Some(min), Some(max)) = (out.min, out.max)
        && min > max
    {
        return Err(format!(
            "the `min` of question `{id}` ({min}) is above its `max` ({max})"
        ));
    }
    if let Some(max) = out.max
        && out.defaults.len() > as_count(max)
    {
        return Err(format!(
            "question `{id}` has {} defaults, above its `max` {max}",
            out.defaults.len()
        ));
    }
    Ok(out)
}

/// The form of a call, with every id and value made and checked.
///
/// # Errors
///
/// What the model got wrong, in words it can act on.
fn normalize(args: Args) -> Result<Form, String> {
    let Args {
        title,
        description,
        choices,
        free_input,
        submit_label,
        ..
    } = args;
    if choices.len() > MAX_QUESTIONS {
        return Err(format!(
            "`choices` has {} questions; at most {MAX_QUESTIONS} fit in one form: ask the rest afterwards",
            choices.len()
        ));
    }
    let mut ids: HashSet<String> = HashSet::new();
    let mut questions = Vec::with_capacity(choices.len());
    for (n, choice) in choices.into_iter().enumerate() {
        questions.push(normalize_question(n + 1, &mut ids, choice)?);
    }
    let free_input = match free_input {
        None | Some(FreeInputArg::Flag(false)) => None,
        Some(FreeInputArg::Flag(true)) => Some(FreeInput {
            label: None,
            placeholder: None,
        }),
        Some(FreeInputArg::Full { label, placeholder }) => Some(FreeInput {
            label: trimmed(label),
            placeholder: trimmed(placeholder),
        }),
    };
    Ok(Form {
        title: trimmed(title),
        description: trimmed(description),
        free_input,
        submit_label: trimmed(submit_label),
        questions,
    })
}

/// The picture of an option as the component takes it.
fn image_json((sha256, alt): &(String, String)) -> Value {
    json!({"sha256": sha256, "alt": alt})
}

/// The `Choices` component instance for `form`, with every member the screen of catalog version 5
/// has: the root of the surface.
fn choices_instance(form: &Form) -> Value {
    let questions: Vec<Value> = form
        .questions
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
                    if let Some(image) = &o.image {
                        option.insert("image".into(), image_json(image));
                    }
                    if let Some(icon) = &o.icon {
                        option.insert("icon".into(), json!(icon));
                    }
                    if o.recommended {
                        option.insert("recommended".into(), json!(true));
                    }
                    Value::Object(option)
                })
                .collect();
            let mut question = Map::new();
            question.insert("id".into(), json!(q.id));
            question.insert("question".into(), json!(q.question));
            if let Some(help) = &q.help {
                question.insert("help".into(), json!(help));
            }
            if let Some(kind) = q.declared {
                question.insert("type".into(), json!(kind.as_str()));
            }
            if !options.is_empty() {
                question.insert("options".into(), Value::Array(options));
            }
            if let Some(multiple) = q.multiple {
                question.insert("multiple".into(), json!(multiple));
            }
            if let Some(other) = q.allow_other {
                question.insert("allowOther".into(), json!(other));
            }
            if let Some(required) = q.required {
                question.insert("required".into(), json!(required));
            }
            match (&q.default_text, q.defaults.as_slice()) {
                (Some(text), _) => {
                    question.insert("default".into(), json!(text));
                }
                (None, [one]) if !q.takes_several() => {
                    question.insert("default".into(), json!(one));
                }
                (None, []) => {}
                (None, many) => {
                    question.insert("default".into(), json!(many));
                }
            }
            if let Some(min) = q.min {
                question.insert("min".into(), json!(min));
            }
            if let Some(max) = q.max {
                question.insert("max".into(), json!(max));
            }
            Value::Object(question)
        })
        .collect();
    let mut component = Map::new();
    component.insert("id".into(), json!("root"));
    component.insert("component".into(), json!("Choices"));
    if let Some(title) = &form.title {
        component.insert("title".into(), json!(title));
    }
    if let Some(description) = &form.description {
        component.insert("description".into(), json!(description));
    }
    if let Some(free) = &form.free_input {
        let mut member = Map::new();
        if let Some(label) = &free.label {
            member.insert("label".into(), json!(label));
        }
        if let Some(placeholder) = &free.placeholder {
            member.insert("placeholder".into(), json!(placeholder));
        }
        component.insert("freeInput".into(), Value::Object(member));
    }
    component.insert("questions".into(), Value::Array(questions));
    if let Some(label) = &form.submit_label {
        component.insert("submitLabel".into(), json!(label));
    }
    component.insert("action".into(), json!({"event": {"name": ANSWER_ACTION}}));
    Value::Object(component)
}

/// `text` cut to `max` characters, with `…` when it was.
fn clipped(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The `Choices` instance for a screen of catalog version 2 to 4, or `None` when the form needs more
/// than that screen has (a typed answer, an option list longer than it takes).
///
/// What the old component has is kept: the title, the questions with radio buttons or checkboxes,
/// the options' labels and descriptions, `allowOther`, `required` and the button's words. The rest is
/// left out or folded in: a question's `help` goes after its text, a recommended option is marked in
/// its label, a deck or a list is a list of radio buttons or checkboxes. The description goes in
/// the question the person reads first (the caller's job).
fn choices_instance_v4(form: &Form) -> Option<Value> {
    let fits = form
        .questions
        .iter()
        .all(|q| !q.kind().is_typed() && q.options.len() <= V4_MAX_OPTIONS);
    if !fits {
        return None;
    }
    let questions: Vec<Value> = form
        .questions
        .iter()
        .map(|q| {
            let options: Vec<Value> = q
                .options
                .iter()
                .map(|o| {
                    let label = if o.recommended {
                        format!("{} (recommended)", clipped(&o.label, MAX_LABEL - 14))
                    } else {
                        o.label.clone()
                    };
                    let mut option = Map::new();
                    option.insert("value".into(), json!(o.value));
                    option.insert("label".into(), json!(label));
                    if let Some(description) = &o.description {
                        option.insert("description".into(), json!(description));
                    }
                    Value::Object(option)
                })
                .collect();
            let text = match &q.help {
                Some(help) => clipped(&format!("{} ({help})", q.question), MAX_QUESTION),
                None => q.question.clone(),
            };
            let mut question = Map::new();
            question.insert("id".into(), json!(q.id));
            question.insert("question".into(), json!(text));
            question.insert("options".into(), Value::Array(options));
            if q.takes_several() {
                question.insert("multiple".into(), json!(true));
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
    let mut component = Map::new();
    component.insert("id".into(), json!("root"));
    component.insert("component".into(), json!("Choices"));
    if let Some(title) = &form.title {
        component.insert("title".into(), json!(title));
    }
    component.insert("questions".into(), Value::Array(questions));
    if let Some(label) = &form.submit_label {
        component.insert("submitLabel".into(), json!(label));
    }
    component.insert("action".into(), json!({"event": {"name": ANSWER_ACTION}}));
    Some(Value::Object(component))
}

/// What marks option number `m` (from 0) of `total`: a letter, else a number.
fn marker(m: usize, total: usize) -> String {
    if total <= 26 {
        char::from(b'a' + u8::try_from(m).unwrap_or(0)).to_string()
    } else {
        (m + 1).to_string()
    }
}

/// `question` followed by the form as text, for a screen that cannot draw it: the person answers in
/// words.
fn as_text(question: &str, form: &Form) -> String {
    let mut text = question.to_owned();
    if let Some(description) = &form.description {
        text.push('\n');
        text.push_str(description);
    }
    text.push('\n');
    for (n, q) in form.questions.iter().enumerate() {
        text.push_str(&format!("\n{}. {}", n + 1, q.question));
        if let Some(help) = &q.help {
            text.push_str(&format!(" ({help})"));
        }
        match q.kind() {
            Kind::Text => text.push_str(" (answer in a few words)"),
            Kind::LongText => text.push_str(" (answer in your own words)"),
            _ if q.takes_several() => text.push_str(" (any number of these)"),
            _ => {}
        }
        for (m, option) in q.options.iter().enumerate() {
            text.push_str(&format!(
                "\n   {}) {}",
                marker(m, q.options.len()),
                option.label
            ));
            if let Some(description) = &option.description {
                text.push_str(&format!(" - {description}"));
            }
            if option.recommended {
                text.push_str(" (recommended)");
            }
            if q.defaults.contains(&option.value) {
                text.push_str(" (default)");
            }
        }
        if q.allow_other == Some(true) {
            text.push_str("\n   or say something else");
        }
    }
    if let Some(free) = &form.free_input {
        let label = free
            .label
            .as_deref()
            .unwrap_or("Anything else you want to add?");
        text.push_str(&format!("\n\n{label} (optional: add it to your reply)"));
    }
    text
}

/// The question the person reads first, on a screen that cannot draw the description: the question,
/// then the description.
fn lead_with_description(question: &str, form: &Form) -> String {
    match &form.description {
        Some(description) => format!("{question}\n{description}"),
        None => question.to_owned(),
    }
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

/// The parameters of `ask_user`, as the model reads them.
fn parameters() -> Value {
    let option = json!({"anyOf": [
        {"type": "string"},
        {"type": "object", "properties": {
            "value": {"type": "string", "pattern": ID_PATTERN, "description": "A short id for the option (1 to 64 letters, digits, - _ . :); made from the label when omitted"},
            "label": {"type": "string", "minLength": 1, "description": "The option's title: a few words"},
            "description": {"type": "string", "description": "The option's subtitle: what it means or what it costs, in a short sentence"},
            "image": {"type": "object", "description": "A picture of the option, for type \"images\": the sha256 of an image file you shared in this conversation, never a URL",
                "properties": {"sha256": {"type": "string", "pattern": "^[0-9a-f]{64}$"}, "alt": {"type": "string", "minLength": 1, "description": "What the picture shows, in words"}},
                "required": ["sha256", "alt"]},
            "icon": {"enum": ICONS, "description": "An icon beside the label"},
            "recommended": {"type": "boolean", "description": "Mark this option as the one you recommend"}},
         "required": ["label"]}]});
    json!({
        "type": "object",
        "properties": {
            "question": {"type": "string", "minLength": 1, "description": "Required, even when `choices` is given: what you need to know, or the sentence that opens the form"},
            "title": {"type": "string", "description": "The form's heading: a few words (with `choices`)"},
            "description": {"type": "string", "description": "Why you are asking, in a sentence or two of plain text, no Markdown (with `choices`)"},
            "choices": {
                "type": "array",
                "maxItems": MAX_QUESTIONS,
                "description": "Questions with fixed answers, asked together as one form (at most 8). Each needs `question` and, unless it is a typed answer, `options`. Omit for an open question.",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "pattern": ID_PATTERN, "description": "A short id for the question (1 to 64 letters, digits, - _ . :), used in the answer; defaults to q1, q2, ..."},
                        "question": {"type": "string", "minLength": 1, "description": "Required: the question"},
                        "help": {"type": "string", "description": "A line under the question: what the options mean, or what the answer is for"},
                        "type": {"enum": ["single", "multiple", "select", "images", "text", "longText"], "description": "How it is answered: single (radio buttons, the default), multiple (checkboxes), select (a searchable list), images (a deck of pictures), text (a short typed answer), longText (a longer one)"},
                        "options": {
                            "type": "array",
                            "maxItems": MAX_SELECT_OPTIONS,
                            "description": "The answers to pick from, each a label or {value, label, description, image, icon, recommended}: 2 to 12 for single and multiple, 2 to 50 for select, 2 to 16 for images (each with an image); none for text and longText",
                            "items": option
                        },
                        "multiple": {"type": "boolean", "description": "Several options may be picked (the same as type \"multiple\"; with select and images it lets several be picked)"},
                        "allowOther": {"type": "boolean", "description": "Offer an \"Other\" the person can write"},
                        "required": {"type": "boolean", "description": "The person must answer this one (the default)"},
                        "default": {"anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}], "description": "What is chosen at first: an option's value or label, or a list of them for several; the words a typed question starts with"},
                        "min": {"type": "integer", "minimum": 0, "description": "The fewest options to pick, when several may be picked"},
                        "max": {"type": "integer", "minimum": 1, "description": "The most options to pick, when several may be picked"}
                    },
                    "required": ["question"]
                }
            },
            "freeInput": {"anyOf": [{"type": "boolean"}, {"type": "object", "properties": {
                "label": {"type": "string", "description": "What the box is called"},
                "placeholder": {"type": "string", "description": "The hint inside the box"}}}],
                "description": "Add an \"anything else\" box to the form; its words come back as the note of the answer"},
            "submitLabel": {"type": "string", "description": "The words of the button (at most 40 characters)"}
        },
        "required": ["question"]
    })
}

#[async_trait]
impl Tool for AskUser {
    /// The step of a call is called "Ask you", and drawn as thinking.
    fn step_style(&self) -> StepStyle {
        StepStyle::default()
            .with_label("Ask you")
            .with_icon(StepIcon::Think)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ASK_USER.to_owned(),
            description: format!("{} {CHOICES_GUIDE}", self.lead.trim()),
            parameters: parameters(),
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
        let question = args.question.trim().to_owned();
        if question.is_empty() {
            return Ok(ToolOutput::error(
                "`question` is required and cannot be empty, even when `choices` is given: it is the \
                 sentence the person reads first",
            ));
        }
        if args.choices.is_empty() {
            return Err(ToolError::needs_input(question));
        }
        let form = match normalize(args) {
            Ok(form) => form,
            Err(problem) => return Ok(ToolOutput::error(problem)),
        };
        match self.state.resolve(ctx.context_map()).await {
            Resolved::Found(catalog) if catalog.component("Choices").is_some() => {
                // The whole form for a screen that has it; else what that screen has, or words.
                let (instance, lead) = if catalog.version() >= CATALOG_V5 {
                    (choices_instance(&form), question)
                } else if let Some(instance) = choices_instance_v4(&form) {
                    tracing::debug!(
                        version = catalog.version(),
                        "the screen's Choices is older than version 5; the form is sent as that screen has it"
                    );
                    (instance, lead_with_description(&question, &form))
                } else {
                    tracing::debug!(
                        version = catalog.version(),
                        "the form needs more than the screen's Choices has; it goes in the text"
                    );
                    return Err(ToolError::needs_input(as_text(&question, &form)));
                };
                if let Err(problem) = catalog.validate(&instance) {
                    return Ok(ToolOutput::error(format!(
                        "the choices do not fit the screen's Choices component: {problem}"
                    )));
                }
                let id = ask_surface_id(ctx.call_id());
                let ui = surface(&id, catalog.catalog_id(), vec![instance]);
                Err(ToolError::needs_input_with_ui(lead, ui))
            }
            Resolved::Found(_) => {
                tracing::debug!("the screen has no Choices component; the options go in the text");
                Err(ToolError::needs_input(as_text(&question, &form)))
            }
            Resolved::NoCatalog => Err(ToolError::needs_input(as_text(&question, &form))),
            Resolved::Unreadable(reason) => {
                tracing::debug!(%reason, "the screen's components could not be read; the options go in the text");
                Err(ToolError::needs_input(as_text(&question, &form)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: Value) -> Args {
        serde_json::from_value(value).unwrap()
    }

    /// The form of a call that asks `choices` and nothing else.
    fn asked(choices: Vec<Value>) -> Result<Form, String> {
        normalize(args(json!({"question": "q", "choices": choices})))
    }

    fn questions(choices: Vec<Value>) -> Vec<Question> {
        asked(choices).unwrap().questions
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
        let questions = questions(vec![
            json!({"question": "Which database?", "options": ["Postgres", "SQLite", "Postgres"]}),
            json!({"id": "auth", "question": " Login? ", "multiple": true, "allowOther": true,
                "options": [{"value": "kc", "label": "Keycloak", "description": "SSO"}, {"label": "None"}]}),
        ]);
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
        let one = |v: Value| asked(vec![v]).unwrap_err();
        assert!(one(json!({"question": "q", "options": ["only"]})).contains("needs 2 to 12"));
        assert!(
            one(json!({"question": "q", "options": (0..13).map(|n| n.to_string()).collect::<Vec<_>>()}))
                .contains("needs 2 to 12")
        );
        assert!(one(json!({"question": " ", "options": ["a", "b"]})).contains("has no text"));
        assert!(one(json!({"question": "q", "options": ["a", " "]})).contains("has no label"));
        assert!(
            one(json!({"question": "q", "options": [{"value": "x", "label": "A"}, {"value": "x", "label": "B"}]}))
                .contains("`x` is used twice")
        );
        let twice = asked(vec![
            json!({"id": "d", "question": "q", "options": ["a", "b"]}),
            json!({"id": "d", "question": "r", "options": ["a", "b"]}),
        ])
        .unwrap_err();
        assert!(twice.contains("`d` is used twice"), "{twice}");
        let many: Vec<Value> = (0..9)
            .map(|n| json!({"question": format!("q{n}"), "options": ["a", "b"]}))
            .collect();
        assert!(asked(many).unwrap_err().contains("at most 8"));
    }

    #[test]
    fn the_limits_by_type_are_the_components() {
        let opts = |n: usize| (0..n).map(|i| format!("Option {i}")).collect::<Vec<_>>();
        let pics = |n: usize| {
            (0..n)
                .map(|i| json!({"label": format!("Tile {i}"), "image": {"sha256": "a".repeat(64), "alt": "x"}}))
                .collect::<Vec<_>>()
        };
        let ok = |kind: &str, options: Vec<Value>| {
            asked(vec![
                json!({"question": "q", "type": kind, "options": options}),
            ])
        };
        let strings = |n: usize| opts(n).into_iter().map(Value::from).collect::<Vec<_>>();
        assert!(ok("single", strings(12)).is_ok());
        assert!(
            ok("single", strings(13))
                .unwrap_err()
                .contains("\"select\" for up to 50")
        );
        assert!(ok("multiple", strings(12)).is_ok());
        assert!(ok("select", strings(50)).is_ok());
        assert!(ok("select", strings(51)).unwrap_err().contains("2 to 50"));
        assert!(ok("select", strings(1)).unwrap_err().contains("2 to 50"));
        assert!(ok("images", pics(16)).is_ok());
        assert!(ok("images", pics(17)).unwrap_err().contains("2 to 16"));
    }

    #[test]
    fn a_deck_needs_a_picture_per_option_and_no_other_question_takes_one() {
        let pic = json!({"sha256": "a".repeat(64), "alt": "A card"});
        let deck = |a: Value, b: Value| {
            asked(vec![
                json!({"question": "q", "type": "images", "options": [a, b]}),
            ])
        };
        assert!(
            deck(
                json!({"label": "A", "image": pic}),
                json!({"label": "B", "image": pic})
            )
            .is_ok()
        );
        let bare = deck(json!({"label": "A", "image": pic}), json!({"label": "B"})).unwrap_err();
        assert!(
            bare.contains("`b`") && bare.contains("has no image"),
            "{bare}"
        );
        let on_radio = asked(vec![
            json!({"question": "q", "options": [{"label": "A", "image": pic}, "B"]}),
        ])
        .unwrap_err();
        assert!(on_radio.contains("only type \"images\""), "{on_radio}");
        for bad in [
            json!({"sha256": "nothex", "alt": "x"}),
            json!({"sha256": "a".repeat(64), "alt": " "}),
        ] {
            let e = deck(
                json!({"label": "A", "image": bad}),
                json!({"label": "B", "image": pic}),
            )
            .unwrap_err();
            assert!(e.contains("never a URL"), "{e}");
        }
        // The hash is written in lower case, whatever the model sent.
        let upper = deck(
            json!({"label": "A", "image": {"sha256": "A".repeat(64), "alt": "x"}}),
            json!({"label": "B", "image": pic}),
        )
        .unwrap();
        assert_eq!(
            upper.questions[0].options[0].image.as_ref().unwrap().0,
            "a".repeat(64)
        );
    }

    #[test]
    fn the_type_and_multiple_may_not_contradict() {
        let one = |v: Value| asked(vec![v]).unwrap_err();
        assert!(
            one(
                json!({"question": "q", "type": "single", "multiple": true, "options": ["a", "b"]})
            )
            .contains("\"single\" but has `multiple: true`")
        );
        assert!(
            one(json!({"question": "q", "type": "multiple", "multiple": false, "options": ["a", "b"]}))
                .contains("\"multiple\" but has `multiple: false`")
        );
        // A list and a deck take several with the flag.
        let list = questions(vec![
            json!({"question": "q", "type": "select", "multiple": true, "options": ["a", "b"]}),
        ]);
        assert!(list[0].takes_several());
        let radio = questions(vec![json!({"question": "q", "options": ["a", "b"]})]);
        assert_eq!(radio[0].kind(), Kind::Single);
        assert!(!radio[0].takes_several());
        let old = questions(vec![
            json!({"question": "q", "multiple": true, "options": ["a", "b"]}),
        ]);
        assert_eq!(old[0].kind(), Kind::Multiple);
    }

    #[test]
    fn a_typed_question_has_no_options_and_may_start_with_words() {
        let typed = |extra: Value| {
            let mut base = json!({"question": "Name?", "type": "text"});
            base.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            asked(vec![base])
        };
        let ok = typed(json!({"default": "  Ada "})).unwrap();
        assert_eq!(ok.questions[0].default_text.as_deref(), Some("Ada"));
        assert!(
            typed(json!({"options": ["a", "b"]}))
                .unwrap_err()
                .contains("takes no options")
        );
        assert!(
            typed(json!({"allowOther": true}))
                .unwrap_err()
                .contains("takes no `multiple`")
        );
        assert!(
            typed(json!({"min": 1}))
                .unwrap_err()
                .contains("takes no `multiple`")
        );
        assert!(
            typed(json!({"default": ["a"]}))
                .unwrap_err()
                .contains("words, not a list")
        );
        assert!(
            typed(json!({"default": "x".repeat(501)}))
                .unwrap_err()
                .contains("over 500")
        );
        // A default has no character the person cannot see: a page may drop it on the way back, and
        // the words that come back would no longer be the default's. A line break is words.
        for hidden in [
            "\u{feff}merge",
            "me\u{200b}rge",
            "merge\u{202e}",
            "a\u{0}b",
            "a\u{ad}b",
        ] {
            let e = typed(json!({"default": hidden})).unwrap_err();
            assert!(e.contains("cannot see"), "{hidden:?}: {e}");
        }
        assert!(typed(json!({"default": "one\ntwo\tthree"})).is_ok());
        assert!(
            asked(vec![
                json!({"question": "q", "type": "longText", "default": "x".repeat(2000)})
            ])
            .is_ok()
        );
    }

    #[test]
    fn a_default_names_an_option_by_value_or_label_and_must_fit() {
        let opts = json!([{"value": "pg", "label": "Postgres"}, {"value": "sq", "label": "SQLite"}, "Mongo"]);
        let one = questions(vec![
            json!({"question": "q", "options": opts, "default": "postgres"}),
        ]);
        assert_eq!(one[0].defaults, ["pg"], "a label, in any case");
        let value = questions(vec![
            json!({"question": "q", "options": opts, "default": "sq"}),
        ]);
        assert_eq!(value[0].defaults, ["sq"]);
        let many = questions(vec![
            json!({"question": "q", "type": "multiple", "options": opts, "default": ["pg", "Mongo"]}),
        ]);
        assert_eq!(many[0].defaults, ["pg", "mongo"]);
        let bad = asked(vec![
            json!({"question": "q", "options": opts, "default": "Oracle"}),
        ])
        .unwrap_err();
        assert!(
            bad.contains("`Oracle`") && bad.contains("not one of its options"),
            "{bad}"
        );
        let two = asked(vec![
            json!({"question": "q", "options": opts, "default": ["pg", "sq"]}),
        ])
        .unwrap_err();
        assert!(two.contains("takes one choice"), "{two}");
    }

    #[test]
    fn min_and_max_count_choices_when_several_may_be_chosen() {
        let q = |extra: Value| {
            let mut base = json!({"question": "q", "type": "multiple", "options": ["a", "b", "c"]});
            base.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            asked(vec![base])
        };
        assert!(q(json!({"min": 1, "max": 3})).is_ok());
        assert!(
            q(json!({"max": 4, "allowOther": true})).is_ok(),
            "an Other is one more choice"
        );
        assert!(q(json!({"max": 4})).unwrap_err().contains("offers 3"));
        assert!(q(json!({"min": 4})).unwrap_err().contains("offers 3"));
        assert!(
            q(json!({"min": 3, "max": 2}))
                .unwrap_err()
                .contains("above its `max`")
        );
        assert!(q(json!({"max": 0})).unwrap_err().contains("offers 3"));
        assert!(
            q(json!({"max": 1, "default": ["a", "b"]}))
                .unwrap_err()
                .contains("above its `max`")
        );
        let single = asked(vec![
            json!({"question": "q", "options": ["a", "b"], "min": 1}),
        ])
        .unwrap_err();
        assert!(
            single.contains("apply when several may be chosen"),
            "{single}"
        );
    }

    fn v5_form() -> Form {
        normalize(args(json!({
            "question": "A few things",
            "title": " Before I start ",
            "description": "The migration depends on them.",
            "freeInput": {"label": "Anything else?", "placeholder": "A deadline"},
            "submitLabel": "Go",
            "choices": [
                {"id": "db", "question": "Which database?", "help": "Where the data lives", "default": "pg",
                 "options": [{"value": "pg", "label": "Postgres", "description": "default", "icon": "git", "recommended": true}, "SQLite"]},
                {"id": "env", "question": "Which environments?", "type": "multiple", "min": 1, "max": 2,
                 "options": ["Dev", "Stage", "Prod"]},
                {"id": "name", "question": "Name?", "type": "text", "default": "Quokka"}
            ]
        })))
        .unwrap()
    }

    #[test]
    fn the_instance_is_a_choices_component_with_an_answer_action() {
        let form = normalize(args(json!({"question": "q", "choices": [{
            "id": "db", "question": "Which database?", "allowOther": true, "required": false,
            "options": [{"value": "pg", "label": "Postgres", "description": "default"}, "SQLite"]}]})))
        .unwrap();
        assert_eq!(
            choices_instance(&form),
            json!({
                "id": "root", "component": "Choices",
                "questions": [{"id": "db", "question": "Which database?",
                    "options": [{"value": "pg", "label": "Postgres", "description": "default"},
                                {"value": "sqlite", "label": "SQLite"}],
                    "allowOther": true, "required": false}],
                "action": {"event": {"name": "answer"}}
            }),
            "a call of version 4 gives the instance of version 4"
        );
    }

    #[test]
    fn the_whole_form_is_sent_to_a_screen_of_version_5() {
        let instance = choices_instance(&v5_form());
        assert_eq!(
            instance,
            json!({
                "id": "root", "component": "Choices",
                "title": "Before I start",
                "description": "The migration depends on them.",
                "freeInput": {"label": "Anything else?", "placeholder": "A deadline"},
                "questions": [
                    {"id": "db", "question": "Which database?", "help": "Where the data lives",
                     "options": [
                        {"value": "pg", "label": "Postgres", "description": "default", "icon": "git", "recommended": true},
                        {"value": "sqlite", "label": "SQLite"}],
                     "default": "pg"},
                    {"id": "env", "question": "Which environments?", "type": "multiple",
                     "options": [{"value": "dev", "label": "Dev"}, {"value": "stage", "label": "Stage"}, {"value": "prod", "label": "Prod"}],
                     "min": 1, "max": 2},
                    {"id": "name", "question": "Name?", "type": "text", "default": "Quokka"}
                ],
                "submitLabel": "Go",
                "action": {"event": {"name": "answer"}}
            })
        );
    }

    #[test]
    fn a_screen_of_version_4_gets_what_it_has() {
        let form = normalize(args(json!({
            "question": "q", "title": "Before I start", "description": "Why I ask.",
            "freeInput": true, "submitLabel": "Go",
            "choices": [
                {"id": "db", "question": "Which database?", "help": "Where the data lives", "type": "select",
                 "options": [{"label": "Postgres", "recommended": true, "icon": "git"}, "SQLite"], "default": "Postgres"},
                {"id": "env", "question": "Environments?", "type": "multiple", "min": 1, "options": ["Dev", "Prod"]}
            ]
        })))
        .unwrap();
        assert_eq!(
            choices_instance_v4(&form).unwrap(),
            json!({
                "id": "root", "component": "Choices", "title": "Before I start",
                "questions": [
                    {"id": "db", "question": "Which database? (Where the data lives)",
                     "options": [{"value": "postgres", "label": "Postgres (recommended)"}, {"value": "sqlite", "label": "SQLite"}]},
                    {"id": "env", "question": "Environments?", "multiple": true,
                     "options": [{"value": "dev", "label": "Dev"}, {"value": "prod", "label": "Prod"}]}
                ],
                "submitLabel": "Go",
                "action": {"event": {"name": "answer"}}
            })
        );
        assert_eq!(
            lead_with_description("q", &form),
            "q\nWhy I ask.",
            "the description goes in the text the person reads first"
        );
    }

    #[test]
    fn a_screen_of_version_4_cannot_take_a_typed_answer_or_a_long_list() {
        assert!(
            choices_instance_v4(&v5_form()).is_none(),
            "a typed question"
        );
        let long = normalize(args(json!({"question": "q", "choices": [
            {"question": "Which?", "type": "select", "options": (0..9).map(|n| format!("Option {n}")).collect::<Vec<_>>()}]})))
        .unwrap();
        assert!(choices_instance_v4(&long).is_none(), "more than 8 options");
    }

    #[test]
    fn the_text_form_lists_the_questions_and_their_options() {
        let form = normalize(args(json!({"question": "Three quick questions", "choices": [
            {"question": "Which database?", "options": ["Postgres", {"label": "SQLite", "description": "embedded"}]},
            {"question": "Where?", "multiple": true, "allowOther": true, "options": ["k8s", "compose"]}
        ]})))
        .unwrap();
        assert_eq!(
            as_text("Three quick questions", &form),
            "Three quick questions\n\
             \n1. Which database?\n   a) Postgres\n   b) SQLite - embedded\
             \n2. Where? (any number of these)\n   a) k8s\n   b) compose\n   or say something else",
            "a call of version 4 reads as it did"
        );
    }

    #[test]
    fn the_text_form_has_the_description_the_help_the_marks_and_the_typed_answers() {
        let text = as_text("A few things", &v5_form());
        assert_eq!(
            text,
            "A few things\nThe migration depends on them.\n\
             \n1. Which database? (Where the data lives)\
             \n   a) Postgres - default (recommended) (default)\
             \n   b) SQLite\
             \n2. Which environments? (any number of these)\
             \n   a) Dev\n   b) Stage\n   c) Prod\
             \n3. Name? (answer in a few words)\
             \n\nAnything else? (optional: add it to your reply)"
        );
        // A long list is numbered, not lettered past z.
        let long = normalize(args(json!({"question": "q", "choices": [
            {"question": "Which?", "type": "select", "options": (0..30).map(|n| format!("Option {n}")).collect::<Vec<_>>()}]})))
        .unwrap();
        let text = as_text("q", &long);
        assert!(
            text.contains("\n   1) Option 0") && text.contains("\n   30) Option 29"),
            "{text}"
        );
    }

    #[test]
    fn the_free_box_is_a_flag_or_words() {
        let form = |free: Value| {
            normalize(args(
                json!({"question": "q", "freeInput": free, "choices": [
            {"question": "Which?", "options": ["a", "b"]}]}),
            ))
            .unwrap()
        };
        assert_eq!(
            form(json!(true)).free_input,
            Some(FreeInput {
                label: None,
                placeholder: None
            })
        );
        assert_eq!(form(json!(false)).free_input, None);
        assert_eq!(
            form(json!({})).free_input,
            Some(FreeInput {
                label: None,
                placeholder: None
            })
        );
        assert_eq!(
            form(json!({"label": " More? "})).free_input,
            Some(FreeInput {
                label: Some("More?".into()),
                placeholder: None
            })
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
            "Ask the person who gave you the task a question. Be specific. `question` is always required"
        ));
        // The constraints are in the description and in the schema, so the first call is right.
        for stated in [
            "`question` is always required",
            "never write the options as a list in your text",
            "`title`",
            "`description` of why you ask",
            "2 to 12",
            "2 to 50",
            "2 to 16",
            "sha256 of an image file you shared",
            "at most 8 questions",
        ] {
            assert!(
                spec.description.contains(stated),
                "{stated}: {}",
                spec.description
            );
        }
        assert_eq!(spec.parameters["required"], json!(["question"]));
        assert_eq!(spec.parameters["properties"]["question"]["minLength"], 1);
        for member in ["title", "description", "freeInput", "submitLabel"] {
            assert!(
                spec.parameters["properties"][member].is_object(),
                "{member}"
            );
        }
        let entry = &spec.parameters["properties"]["choices"]["items"];
        assert_eq!(entry["required"], json!(["question"]));
        assert_eq!(entry["properties"]["id"]["pattern"], ID_PATTERN);
        assert_eq!(
            entry["properties"]["type"]["enum"],
            json!(["single", "multiple", "select", "images", "text", "longText"])
        );
        assert_eq!(spec.parameters["properties"]["choices"]["maxItems"], 8);
        let option = &entry["properties"]["options"];
        assert_eq!(option["maxItems"], 50);
        assert!(option["items"]["anyOf"].is_array());
        let full = &option["items"]["anyOf"][1]["properties"];
        assert_eq!(full["icon"]["enum"], json!(ICONS));
        assert_eq!(full["image"]["required"], json!(["sha256", "alt"]));
    }

    #[test]
    fn the_icons_are_the_ids_of_steps_v1() {
        assert_eq!(ICONS.len(), 15);
        assert!(ICONS.contains(&"opencode") && ICONS.contains(&"git"));
        // Every id is a plain lower-case word.
        assert!(
            ICONS
                .iter()
                .all(|i| i.bytes().all(|b| b.is_ascii_lowercase()))
        );
    }
}
