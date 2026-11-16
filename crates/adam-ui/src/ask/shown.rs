//! A question that shows what it asks about (catalog version 8): `show: [{component, ...}]` on a
//! question of `ask_user`, a few components drawn with the question, between its words and its
//! control.
//!
//! The model writes the components, adam-ui gives them their ids (`q<i>-s<j>`: question `i`, entry
//! `j`) and puts them in the surface; the screen's `Choices` names them by id in its own `show`. What
//! the contract refuses is refused here before anything is drawn, on every screen and with no
//! catalog, naming the block and the member: a component off the closed list of seven, more than 4 in
//! a question or 8 in a form, a member missing, of the wrong type or past a limit of its component
//! (`blocks.rs` holds the table, a test holds it equal to the catalog of version 8), an `Image` with
//! no `alt`, a `Chart` series with other than one value per label.
//!
//! A control or format character in any text of a block (a zero-width or bidirectional one among them)
//! is refused, because a confirmation that shows a diff is read by a person who trusts what is drawn.
//! A tab and a line break are text. Blank characters that are neither control nor format (U+2028 and
//! U+2029, U+034F, the variation selectors, U+115F, U+1160, U+3164, U+FFA0, U+2800) are not refused.
//!
//! What a screen does with it is read from the screen's `Choices` schema, not from its version:
//!
//! * **it takes `show`:** the components are loose in the surface and the question names them.
//! * **it does not** (versions 2 to 7): the components go in a `Column` **above** the form, each
//!   question's under a caption that names the question and the blocks, as the contract advises for a
//!   client of version 7 (one form; the captions are this crate's).
//! * **it has no `Column`, or lacks the component:** the content goes **in words** after the question
//!   the person reads first, under the same caption, and nothing is left half placed.
//!
//! The blocks of a question are numbered in the order the model listed them, and a caption or a
//! `Block <n>:` says the number wherever the block lands. The model's `help` is never touched.

use std::collections::HashSet;
use std::fmt::Write as _;

use adam_a2a_runtime::is_format_or_control;
use serde_json::{Map, Value, json};

use super::reask::QUOTED_QUESTION;
use super::{Form, Question, blocks, clipped};
use crate::catalog::Catalog;

/// The components a question may show: the contract's closed list.
pub(super) const SHOWABLE: [&str; 7] = [
    "Text", "Image", "Mermaid", "Code", "Cards", "Callout", "Chart",
];
/// The most components one question shows.
pub(super) const MAX_PER_QUESTION: usize = 4;
/// The most components one form shows in all.
pub(super) const MAX_PER_FORM: usize = 8;
/// The most bytes of the operations of a surface, written as JSON: what the screen reads.
const MAX_SURFACE_BYTES: usize = 64 * 1024;
/// The id the `Choices` has when a `Column` is the root.
const FORM_ID: &str = "ask";

/// Where a shown component is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Placed {
    /// Loose in the surface, named by the question's `show`.
    Native,
    /// In the `Column` above the form, for a screen whose `Choices` has no `show`.
    Above,
    /// Written in the words the person reads first.
    Words,
}

/// A component a question shows, checked for shape: the model's own members, no `id`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Shown {
    pub(super) id: String,
    pub(super) position: usize,
    pub(super) component: String,
    props: Map<String, Value>,
    pub(super) placed: Placed,
}

impl Shown {
    /// The component as the surface holds it.
    fn instance(&self) -> Value {
        let mut component = self.props.clone();
        component.insert("id".into(), json!(self.id));
        Value::Object(component)
    }
}

/// The `show` of question `id` (number `n`), checked: what the model got wrong comes back in words
/// it can act on, before anything is drawn.
pub(super) fn parse(id: &str, n: usize, given: Option<Value>) -> Result<Vec<Shown>, String> {
    let Some(given) = given else {
        return Ok(Vec::new());
    };
    let Value::Array(entries) = given else {
        return Err(format!(
            "`show` of question `{id}` is not a list of blocks like [{{\"component\": \"Code\", \"code\": \"...\"}}]"
        ));
    };
    if entries.is_empty() || entries.len() > MAX_PER_QUESTION {
        return Err(format!(
            "`show` of question `{id}` has {} blocks; it needs 1 to {MAX_PER_QUESTION}, or leave it out",
            entries.len()
        ));
    }
    entries
        .into_iter()
        .enumerate()
        .map(|(j, entry)| one(id, n, j + 1, entry))
        .collect()
}

fn one(id: &str, n: usize, position: usize, entry: Value) -> Result<Shown, String> {
    let at = format!("block {position} of `show` of question `{id}`");
    let Value::Object(mut props) = entry else {
        return Err(format!("{at} is not an object {{\"component\": ..., ...}}"));
    };
    let Some(component) = props
        .get("component")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Err(format!(
            "{at} has no `component`; a question shows {}",
            SHOWABLE.join(", ")
        ));
    };
    if !SHOWABLE.contains(&component.as_str()) {
        return Err(format!(
            "{at} is a `{}`, which a question cannot show: it shows {}, which hold nothing else and take no \
             answer (put a table or a diff in a `Code` block)",
            clipped(&component, 40),
            SHOWABLE.join(", ")
        ));
    }
    if component == "Image" {
        let alt = props
            .get("alt")
            .and_then(Value::as_str)
            .is_some_and(|alt| !alt.trim().is_empty());
        if !alt {
            return Err(format!(
                "{at} is an `Image` with no `alt`: say what it shows in words, it is never decoration"
            ));
        }
        if let Some(Value::String(hash)) = props.get_mut("artifact") {
            *hash = hash.trim().to_ascii_lowercase();
        }
    }
    blocks::check(&component, &props)
        .map_err(|problem| format!("{at} ({component}): {problem}"))?;
    for (member, value) in &props {
        plain(&at, member, value)?;
    }
    Ok(Shown {
        id: format!("q{n}-s{position}"),
        position,
        component,
        props,
        placed: Placed::Native,
    })
}

/// `value` has no control or format character in any text (a line break and a tab are text): a
/// diff or a graph is read by a person who trusts that what is drawn is what is there.
fn plain(at: &str, path: &str, value: &Value) -> Result<(), String> {
    match value {
        Value::String(text) => match text
            .chars()
            .find(|c| !matches!(c, '\t' | '\n' | '\r') && is_format_or_control(*c))
        {
            Some(hidden) => Err(format!(
                "{at} has a character the person cannot see (U+{:04X}) in `{path}`: a control or format \
                 character is refused. Write it as a visible escape such as \\u{{{:04X}}}, or say it in words",
                u32::from(hidden),
                u32::from(hidden)
            )),
            None => Ok(()),
        },
        Value::Array(items) => items
            .iter()
            .enumerate()
            .try_for_each(|(n, item)| plain(at, &format!("{path}[{}]", n + 1), item)),
        Value::Object(members) => members
            .iter()
            .try_for_each(|(member, item)| plain(at, &format!("{path}.{member}"), item)),
        _ => Ok(()),
    }
}

/// The form shows no more than a form may.
pub(super) fn check_total(form: &Form) -> Result<(), String> {
    let total: usize = form.questions.iter().map(|q| q.shown.len()).sum();
    if total > MAX_PER_FORM {
        return Err(format!(
            "the form shows {total} blocks in all; at most {MAX_PER_FORM} fit in one form: show less, or ask the rest afterwards"
        ));
    }
    Ok(())
}

/// Whether the screen's `Choices` takes a `show` on a question: asked of its schema.
fn takes_show(catalog: &Catalog) -> bool {
    catalog
        .validate(
            &json!({"id": "probe", "component": "Choices", "questions": [{
            "id": "probe", "question": "x",
            "options": [{"value": "a", "label": "A"}, {"value": "b", "label": "B"}],
            "show": ["probe-s1"]}]}),
        )
        .is_ok()
}

/// What a form that shows something is made of on one screen.
pub(super) struct Arranged {
    /// The form with where each component went.
    pub(super) form: Form,
    column: Option<Value>,
    above: Vec<Value>,
    loose: Vec<Value>,
    /// What the screen cannot draw, in words, to follow the question the person reads first.
    pub(super) words: String,
}

impl Arranged {
    /// The components of the surface with `choices` (the form) in it: the `Choices` alone, or a
    /// `Column` as the root with the form, what is above it and what is loose.
    pub(super) fn assemble(&self, mut choices: Value) -> Vec<Value> {
        let Some(column) = &self.column else {
            return vec![choices];
        };
        choices["id"] = json!(FORM_ID);
        let mut components = vec![column.clone(), choices];
        components.extend(self.above.iter().cloned());
        components.extend(self.loose.iter().cloned());
        components
    }
}

/// The caption of the blocks of question `n` that are written or drawn apart from it.
fn caption(n: usize, question: &Question, positions: &[usize]) -> String {
    let numbers = match positions {
        [one] => one.to_string(),
        [rest @ .., last] => format!(
            "{} and {last}",
            rest.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        [] => String::new(),
    };
    format!(
        "For question {n} (\"{}\"), block{} {numbers} of {}:",
        clipped(&question.question, QUOTED_QUESTION),
        if positions.len() == 1 { "" } else { "s" },
        question.shown.len()
    )
}

/// The caption component placed above the form for question `n`.
fn caption_component(n: usize, text: &str) -> Value {
    json!({"id": format!("q{n}-cap"), "component": "Text", "text": text})
}

/// The words of the blocks of question `n` that `said` picks, under their caption, or nothing.
fn words_of(n: usize, question: &Question, said: impl Fn(&Shown) -> bool) -> String {
    let blocks: Vec<&Shown> = question.shown.iter().filter(|s| said(s)).collect();
    if blocks.is_empty() {
        return String::new();
    }
    let positions: Vec<usize> = blocks.iter().map(|s| s.position).collect();
    let mut words = format!("\n\n{}", caption(n, question, &positions));
    for shown in blocks {
        let _ = write!(words, "\nBlock {}: {}", shown.position, describe(shown));
    }
    words
}

/// `form` as the screen of `catalog` draws what it shows, or `None` when it shows nothing. What the
/// model got wrong in a component (the screen's own schema says) is an error naming the block.
pub(super) fn arrange(catalog: &Catalog, form: &Form) -> Result<Option<Arranged>, String> {
    if form.questions.iter().all(|q| q.shown.is_empty()) {
        return Ok(None);
    }
    let native = takes_show(catalog);
    let mut form = form.clone();
    for q in &mut form.questions {
        for shown in &mut q.shown {
            if catalog.component(&shown.component).is_none() {
                shown.placed = Placed::Words;
                continue;
            }
            catalog.validate(&shown.instance()).map_err(|problem| {
                format!(
                    "block {} of `show` of question `{}` ({}): {problem}",
                    shown.position, q.id, shown.component
                )
            })?;
            shown.placed = if native {
                Placed::Native
            } else {
                Placed::Above
            };
        }
    }
    // What goes above the form is under a caption of its own; a screen whose `Text` does not take
    // the caption has the question's blocks in words.
    let mut captions: Vec<Option<Value>> = Vec::new();
    for (i, q) in form.questions.iter_mut().enumerate() {
        let positions: Vec<usize> = q
            .shown
            .iter()
            .filter(|s| s.placed == Placed::Above)
            .map(|s| s.position)
            .collect();
        let component = (!positions.is_empty())
            .then(|| caption_component(i + 1, &caption(i + 1, q, &positions)))
            .filter(|component| catalog.validate(component).is_ok());
        if component.is_none() {
            for shown in &mut q.shown {
                if shown.placed == Placed::Above {
                    shown.placed = Placed::Words;
                }
            }
        }
        captions.push(component);
    }
    let mut children = Vec::new();
    for (q, caption) in form.questions.iter().zip(&captions) {
        if let Some(caption) = caption {
            children.push(caption["id"].as_str().unwrap_or_default().to_owned());
        }
        children.extend(
            q.shown
                .iter()
                .filter(|s| s.placed == Placed::Above)
                .map(|s| s.id.clone()),
        );
    }
    let drawn = form
        .questions
        .iter()
        .flat_map(|q| &q.shown)
        .any(|s| s.placed != Placed::Words);
    children.push(FORM_ID.to_owned());
    let column = json!({"id": "root", "component": "Column", "children": children});
    let column = (drawn && catalog.validate(&column).is_ok()).then_some(column);
    if column.is_none() {
        // Nothing is half placed: with no `Column` there is nowhere to put a block.
        for shown in form.questions.iter_mut().flat_map(|q| &mut q.shown) {
            shown.placed = Placed::Words;
        }
        captions.iter_mut().for_each(|caption| *caption = None);
    }
    let (mut above, mut loose) = (Vec::new(), Vec::new());
    let mut words = String::new();
    for ((i, q), caption) in form.questions.iter().enumerate().zip(captions) {
        above.extend(caption);
        for shown in &q.shown {
            match shown.placed {
                Placed::Above => above.push(shown.instance()),
                Placed::Native => loose.push(shown.instance()),
                Placed::Words => {}
            }
        }
        words.push_str(&words_of(i + 1, q, |s| s.placed == Placed::Words));
    }
    Ok(Some(Arranged {
        form,
        column,
        above,
        loose,
        words,
    }))
}

/// The surface is no larger than a screen reads.
pub(super) fn check_size(messages: &Value) -> Result<(), String> {
    let size = messages.to_string().len();
    if size > MAX_SURFACE_BYTES {
        return Err(format!(
            "what the form shows is too large: its surface is {size} bytes and a screen reads at most \
             {MAX_SURFACE_BYTES}. Show less, or shorter code"
        ));
    }
    Ok(())
}

/// The words of what a form shows are no longer than the surface of one may be.
pub(super) fn check_words(words: &str) -> Result<(), String> {
    if words.len() > MAX_SURFACE_BYTES {
        return Err(format!(
            "what the form shows is too large: written out it is {} bytes and at most {MAX_SURFACE_BYTES} \
             are read. Show less, or shorter code",
            words.len()
        ));
    }
    Ok(())
}

/// What every question of `form` shows, in words: for a form asked as text.
pub(super) fn all_words(form: &Form) -> String {
    form.questions.iter().map(as_text).collect::<String>()
}

/// What a question shows, in words, to follow the question in a form asked as text: each block under
/// its number.
pub(super) fn as_text(question: &Question) -> String {
    let mut text = String::new();
    for shown in &question.shown {
        // Not indented: code is shown as it is, and a diff keeps its first characters.
        let _ = write!(
            text,
            "\nBlock {} of {}: {}",
            shown.position,
            question.shown.len(),
            describe(shown)
        );
    }
    text
}

/// What a re-asked form shows that its surface does not hold, in words: the question the person
/// reads first must still say it.
pub(super) fn undrawn_words(form: &Form, ui: &Value) -> String {
    let drawn: HashSet<&str> = ui
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| {
            message
                .get("updateComponents")?
                .get("components")?
                .as_array()
        })
        .flatten()
        .filter_map(|component| component.get("id")?.as_str())
        .collect();
    form.questions
        .iter()
        .enumerate()
        .map(|(i, q)| words_of(i + 1, q, |s| !drawn.contains(s.id.as_str())))
        .collect()
}

fn text_of<'a>(props: &'a Map<String, Value>, member: &str) -> Option<&'a str> {
    props
        .get(member)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// The fence of a block of code: longer than any run of backticks in it, so that the code cannot end
/// the block early.
fn fenced(code: &str, info: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in code.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{info}\n{code}\n{fence}")
}

/// A shown component as words and, for code, as the code: every member that is content, nothing cut.
/// The members that only say how to draw (`weight`, `Text.variant`, `Cards.layout`) are not said.
fn describe(shown: &Shown) -> String {
    let props = &shown.props;
    let say = |member: &str| text_of(props, member);
    let titled = |what: &str, title: Option<&str>| match title {
        Some(title) => format!("{what} \"{title}\""),
        None => what.to_owned(),
    };
    match shown.component.as_str() {
        "Text" => say("text").unwrap_or_default().to_owned(),
        "Callout" => format!(
            "{} - {}: {}",
            say("tone").unwrap_or("info"),
            say("title").unwrap_or_default(),
            say("text").unwrap_or_default()
        ),
        "Code" | "Mermaid" => {
            let (mut what, language) = if shown.component == "Code" {
                (titled("Code", say("title")), say("language"))
            } else {
                (titled("A diagram", say("title")), Some("mermaid"))
            };
            let details: Vec<String> = [
                say("filename").map(|file| format!("file {file}")),
                language
                    .filter(|_| shown.component == "Code")
                    .map(|language| format!("language {language}")),
            ]
            .into_iter()
            .flatten()
            .collect();
            if !details.is_empty() {
                let _ = write!(what, " ({})", details.join(", "));
            }
            // The language names the fence only when it is a plain word.
            let info = language
                .filter(|l| {
                    l.chars().all(|c| {
                        c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '_' | '.' | '#')
                    })
                })
                .unwrap_or_default();
            let code = props
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut text = format!("{what}:\n{}", fenced(code, info));
            if let Some(caption) = say("caption") {
                let _ = write!(text, "\n{caption}");
            }
            text
        }
        "Cards" => {
            let mut text = titled("Cards", say("title"));
            text.push(':');
            for card in props
                .get("cards")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let member =
                    |name: &str| card.get(name).and_then(Value::as_str).unwrap_or_default();
                let _ = write!(text, "\n- {}", member("title"));
                for (name, open, close) in [
                    ("subtitle", " (", ")"),
                    ("body", ": ", ""),
                    ("url", " <", ">"),
                ] {
                    if !member(name).is_empty() {
                        let _ = write!(text, "{open}{}{close}", member(name));
                    }
                }
                let tags: Vec<&str> = card
                    .get("tags")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                if !tags.is_empty() {
                    let _ = write!(text, " [tags: {}]", tags.join(", "));
                }
            }
            text
        }
        "Chart" => {
            let unit = say("unit")
                .map(|unit| format!(" in {unit}"))
                .unwrap_or_default();
            let kind = say("type").unwrap_or("bar");
            let mut text = format!(
                "{}{unit}:",
                titled(&format!("A {kind} chart"), say("title"))
            );
            let labels: Vec<&str> = props
                .get("labels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            for series in props
                .get("series")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let name = series
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let values = series
                    .get("values")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten();
                let points: Vec<String> = labels
                    .iter()
                    .zip(values)
                    .map(|(label, value)| format!("{label} {value}"))
                    .collect();
                let _ = write!(text, "\n- {name}: {}", points.join(", "));
            }
            if let Some(caption) = say("caption") {
                let _ = write!(text, "\n{caption}");
            }
            text
        }
        // `Image`: the person has the file the model shared; the screen cannot draw it here.
        _ => {
            let mut text = format!("A picture: {}", say("alt").unwrap_or_default());
            if let Some(caption) = say("caption") {
                let _ = write!(text, " ({caption})");
            }
            text.push_str(" - a file shared in this conversation");
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message whose JSON text is `bytes` long: the quotes are two of them.
    fn message(text: &str) -> Value {
        Value::String(text.to_owned())
    }

    #[test]
    fn the_bound_of_a_surface_is_in_utf8_bytes_exactly() {
        assert!(check_size(&message(&"x".repeat(MAX_SURFACE_BYTES - 2))).is_ok());
        assert!(check_size(&message(&"x".repeat(MAX_SURFACE_BYTES - 1))).is_err());
        // 21845 three-byte characters are 65535 bytes and 21845 characters.
        assert!(check_size(&message(&"€".repeat(21844))).is_ok());
        assert!(check_size(&message(&"€".repeat(21845))).is_err());
        assert!(check_size(&message(&format!("{}é", "x".repeat(MAX_SURFACE_BYTES - 3)))).is_err());
    }

    #[test]
    fn the_bound_of_words_is_in_utf8_bytes_exactly() {
        assert!(check_words(&"x".repeat(MAX_SURFACE_BYTES)).is_ok());
        assert!(check_words(&"x".repeat(MAX_SURFACE_BYTES + 1)).is_err());
        assert!(check_words(&"€".repeat(21845)).is_ok());
        assert!(check_words(&"€".repeat(21846)).is_err());
    }
}
