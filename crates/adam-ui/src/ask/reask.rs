//! The agent's side of an answer: judge it against the form the call asked
//! ([`Tool::read_answer`](adam_llm_agent::Tool::read_answer)), and when it breaks a rule, ask the
//! same form again on a new surface with what the person got right already filled in.
//!
//! The form is rebuilt from the call's own arguments, which the history already holds; what was
//! drawn is read from the interface that went with the question, so a kind the screen did not have
//! (asked as the nearest older form) is judged by the shape it was answered in.

use std::collections::HashMap;

use adam_a2a_runtime::{PAYLOAD_FORM, is_format_or_control};
use adam_llm_agent::{AnswerCheck, Refusal};
use serde_json::{Value, json};

use super::answer::{Check, FormSpec, Verdict, check_answer};
use super::{
    Args, Form, Kind, MAX_LONG_TEXT, MAX_TEXT, clipped, kinds, lead_with_description, normalize,
};
use crate::surface::{ask_attempt, ask_retry_surface_id};

/// The longest words of a question quoted back to the person.
const QUOTED_QUESTION: usize = 80;

/// What `ask_user` says about the answer to its call `call_id`: [`AnswerCheck::Accept`] unless the
/// person answered a form and the answer breaks a rule of the catalog.
pub(super) fn read(
    call_id: &str,
    args: &Value,
    ui: Option<&Value>,
    payload: &Value,
) -> AnswerCheck {
    let (Some(ui), Some(answered)) = (ui, payload.get(PAYLOAD_FORM)) else {
        return AnswerCheck::Accept;
    };
    let Ok(parsed) = serde_json::from_value::<Args>(args.clone()) else {
        return AnswerCheck::Accept;
    };
    let question = parsed.question.trim().to_owned();
    let Ok(form) = normalize(parsed) else {
        return AnswerCheck::Accept;
    };
    let Some(drawn) = Drawn::of(ui) else {
        return AnswerCheck::Accept;
    };
    let spec = spec_of(&form, &drawn);
    if !spec.judges_anything() {
        return AnswerCheck::Accept;
    }
    let context = answered.get("context").unwrap_or(&Value::Null);
    match check_answer(&spec, context) {
        Ok(()) => AnswerCheck::Accept,
        Err(verdict) => AnswerCheck::Refuse(ask_again(
            call_id, &question, &form, &drawn, ui, context, &verdict,
        )),
    }
}

/// The form as it was drawn: the interface's messages, and the kind each question was drawn as.
struct Drawn {
    /// Each question's `type` in the `Choices` component, by id; a question with none is `single`
    /// (or `multiple` when it says so).
    types: HashMap<String, String>,
}

impl Drawn {
    fn of(ui: &Value) -> Option<Self> {
        let root = root_of(ui)?;
        let types = root
            .get("questions")?
            .as_array()?
            .iter()
            .filter_map(|q| {
                let id = q.get("id")?.as_str()?.to_owned();
                let kind = match q.get("type").and_then(Value::as_str) {
                    Some(kind) => kind.to_owned(),
                    None if q.get("multiple") == Some(&json!(true)) => "multiple".to_owned(),
                    None => "single".to_owned(),
                };
                Some((id, kind))
            })
            .collect();
        Some(Self { types })
    }
}

/// The `Choices` component of the interface: the component with the id `root` of its
/// `updateComponents` message.
fn root_of(ui: &Value) -> Option<&Value> {
    ui.as_array()?.iter().find_map(|message| {
        message
            .get("updateComponents")?
            .get("components")?
            .as_array()?
            .iter()
            .find(|c| c.get("id").and_then(Value::as_str) == Some("root"))
    })
}

fn root_of_mut(ui: &mut Value) -> Option<&mut Value> {
    ui.as_array_mut()?.iter_mut().find_map(|message| {
        message
            .get_mut("updateComponents")?
            .get_mut("components")?
            .as_array_mut()?
            .iter_mut()
            .find(|c| c.get("id").and_then(Value::as_str) == Some("root"))
    })
}

/// What the answer is judged by: each of the five kinds as it was drawn. A kind drawn as itself is
/// judged by the contract; a yes or no and a scale asked as radio buttons are answered in the same
/// shape (`values`), so they are judged by it too; any other kind that the screen lacked came back
/// as typed words and is left to the model, as the older types are.
fn spec_of(form: &Form, drawn: &Drawn) -> FormSpec {
    let questions = form
        .questions
        .iter()
        .map(|q| {
            let mut spec = kinds::spec_of(q);
            let shown = drawn.types.get(&q.id).map(String::as_str);
            let kind = q.kind();
            let judged = !kind.is_extended()
                || shown == Some(kind.as_str())
                || (matches!(kind, Kind::Confirm | Kind::Scale) && shown == Some("single"));
            if !judged {
                spec.check = Check::Unchecked;
            }
            spec
        })
        .collect();
    FormSpec {
        free_input: form.free_input.is_some(),
        questions,
    }
}

/// The new question: what was wrong, the question again, and the same form on a new surface with
/// what the person got right as the defaults.
fn ask_again(
    call_id: &str,
    question: &str,
    form: &Form,
    drawn: &Drawn,
    ui: &Value,
    context: &Value,
    verdict: &Verdict,
) -> Refusal {
    let rule = verdict.failure.rule.as_str();
    let said = match &verdict.question {
        Some(id) => {
            let words = form
                .questions
                .iter()
                .find(|q| &q.id == id)
                .map(|q| clipped(&q.question, QUOTED_QUESTION))
                .unwrap_or_default();
            format!(
                "The answer to \"{id}\" (\"{words}\") was not used ({rule}): {}.",
                verdict.failure.why
            )
        }
        None => format!(
            "The answers were not used ({rule}): {}.",
            verdict.failure.why
        ),
    };
    let mut asked = ui.clone();
    let folded = form.description.is_some()
        && root_of(ui).is_some_and(|root| root.get("description").is_none());
    let lead = if folded {
        lead_with_description(question, form)
    } else {
        question.to_owned()
    };
    let attempt = surface_of(ui)
        .and_then(|surface| ask_attempt(&surface))
        .map_or(1, |n| n + 1);
    let surface = ask_retry_surface_id(call_id, attempt);
    set_surface(&mut asked, &surface);
    carry_answers(&mut asked, drawn, context, &verdict.valid);
    let refusal = Refusal::new(format!("{said}\n{lead}"), Some(asked), rule);
    match &verdict.question {
        Some(id) => refusal.about(id.clone()),
        None => refusal,
    }
}

/// The id of the surface the interface creates.
fn surface_of(ui: &Value) -> Option<String> {
    ui.as_array()?.iter().find_map(|message| {
        message
            .get("createSurface")?
            .get("surfaceId")?
            .as_str()
            .map(str::to_owned)
    })
}

/// Name `surface` in every message of the interface.
fn set_surface(ui: &mut Value, surface: &str) {
    let Some(messages) = ui.as_array_mut() else {
        return;
    };
    for message in messages {
        for key in ["createSurface", "updateComponents"] {
            if let Some(Value::Object(body)) = message.get_mut(key) {
                body.insert("surfaceId".into(), json!(surface));
            }
        }
    }
}

/// What the person answered to a question that passed, as the question's `default`, so that
/// asking again loses nothing they gave.
fn carry_answers(ui: &mut Value, drawn: &Drawn, context: &Value, valid: &[bool]) {
    let (Some(entries), Some(root)) = (
        context.get("answers").and_then(Value::as_array),
        root_of_mut(ui),
    ) else {
        return;
    };
    let Some(questions) = root.get_mut("questions").and_then(Value::as_array_mut) else {
        return;
    };
    for ((question, entry), ok) in questions.iter_mut().zip(entries).zip(valid) {
        if !ok {
            continue;
        }
        let kind = question
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| drawn.types.get(id))
            .cloned()
            .unwrap_or_default();
        if let (Some(default), Value::Object(member)) =
            (default_of(&kind, question, entry), question)
        {
            member.insert("default".into(), default);
        }
    }
}

/// The answer in `entry` as the `default` of a question drawn as `kind`, or `None` when it was
/// not answered or is not one this question can start from.
fn default_of(kind: &str, question: &Value, entry: &Value) -> Option<Value> {
    let values: Vec<&str> = entry
        .get("values")?
        .as_array()?
        .iter()
        .map(Value::as_str)
        .collect::<Option<_>>()?;
    let offered = |value: &str| {
        question
            .get("options")
            .and_then(Value::as_array)
            .is_some_and(|options| {
                options
                    .iter()
                    .any(|o| o.get("value").and_then(Value::as_str) == Some(value))
            })
    };
    let several = question.get("multiple") == Some(&json!(true));
    match (kind, values.as_slice()) {
        (_, []) if matches!(kind, "text" | "longText") => {
            let text = entry.get("text")?.as_str()?;
            let most = if kind == "text" {
                MAX_TEXT
            } else {
                MAX_LONG_TEXT
            };
            // The words start the box again as long as they show: a typed default with a
            // character nobody can see is refused when a form is drawn.
            let hidden = text
                .chars()
                .any(|c| !matches!(c, '\t' | '\n' | '\r') && is_format_or_control(c));
            (!text.is_empty() && text.chars().count() <= most && !hidden).then(|| json!(text))
        }
        ("confirm" | "number" | "scale" | "date", [one]) => Some(json!(one)),
        ("rank", list) if !list.is_empty() => Some(json!(list)),
        ("single", [one]) if offered(one) => Some(json!(one)),
        ("select" | "images", [one]) if offered(one) && !several => Some(json!(one)),
        ("multiple" | "select" | "images", list)
            if !list.is_empty()
                && list.iter().all(|v| offered(v))
                && (kind == "multiple" || several) =>
        {
            Some(json!(list))
        }
        _ => None,
    }
}
