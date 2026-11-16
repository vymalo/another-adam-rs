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

use super::answer::{Check, FormSpec, Rule, Verdict, check_answer, whole};
use super::shown;
use super::{
    Args, Form, Kind, MAX_LONG_TEXT, MAX_TEXT, clipped, kinds, lead_with_description, normalize,
};
use crate::surface::{ask_attempt, ask_retry_surface_id};

/// The longest words of a question quoted back to the person.
pub(super) const QUOTED_QUESTION: usize = 80;

/// How many times a form is asked again after an answer that does not fit. The next bad answer ends
/// the question: the model is told, as an error, and may ask another way.
pub(crate) const MAX_REFUSALS: u32 = 5;

/// What `ask_user` says about the answer to its call `call_id`: [`AnswerCheck::Accept`] unless the
/// person answered a form and the answer breaks a rule of the catalog (or is for another form, or
/// holds several answers), and a yes or no said in words as the one question of a form that went out
/// as text.
pub(super) fn read(
    call_id: &str,
    args: &Value,
    ui: Option<&Value>,
    payload: &Value,
) -> AnswerCheck {
    let Ok(parsed) = serde_json::from_value::<Args>(args.clone()) else {
        return AnswerCheck::Accept;
    };
    let question = parsed.question.trim().to_owned();
    let Ok(form) = normalize(parsed) else {
        return AnswerCheck::Accept;
    };
    let Some(ui) = ui else {
        return in_words(&form, payload);
    };
    let (Some(answered), Some(drawn)) = (payload.get(PAYLOAD_FORM), Drawn::of(ui)) else {
        return AnswerCheck::Accept;
    };
    let spec = spec_of(&form, &drawn);
    let context = answered.get("context").unwrap_or(&Value::Null);
    let Some(verdict) = judge(&spec, &drawn, ui, answered, context) else {
        return AnswerCheck::Accept;
    };
    let tries = surface_of(ui)
        .and_then(|surface| ask_attempt(&surface))
        .unwrap_or(0);
    if tries >= MAX_REFUSALS {
        return AnswerCheck::End(giving_up(&form, &verdict));
    }
    AnswerCheck::Refuse(ask_again(
        call_id, &question, &form, &drawn, ui, context, &verdict,
    ))
}

/// Why the answer in `answered` is not used, or `None` when it is.
fn judge(
    spec: &FormSpec,
    drawn: &Drawn,
    ui: &Value,
    answered: &Value,
    context: &Value,
) -> Option<Verdict> {
    if answered
        .get("actions")
        .and_then(Value::as_u64)
        .is_some_and(|n| n > 1)
    {
        return Some(whole(
            Rule::SeveralAnswers,
            "the message holds more than one answer to a form",
        ));
    }
    let asked = surface_of(ui);
    if asked.is_some() && answered.get("surface").and_then(Value::as_str) != asked.as_deref() {
        return Some(whole(
            Rule::OtherSurface,
            "the answer is for another form than the one asked",
        ));
    }
    if !spec.judges_anything() {
        return None;
    }
    check_answer(spec, &typed_as_values(spec, drawn, context)).err()
}

/// A yes or no as the one question of a form that went out as words: the person's reply is that
/// answer when it is exactly `yes`, `no` or one of the two labels, in any case. Anything else is
/// words, and the model reads it.
fn in_words(form: &Form, payload: &Value) -> AnswerCheck {
    let [question] = form.questions.as_slice() else {
        return AnswerCheck::Accept;
    };
    if question.kind() != Kind::Confirm || payload.get(PAYLOAD_FORM).is_some() {
        return AnswerCheck::Accept;
    }
    let Some(text) = payload.get("text").and_then(Value::as_str) else {
        return AnswerCheck::Accept;
    };
    kinds::confirm_answer(
        text,
        question.ext.yes_label.as_deref(),
        question.ext.no_label.as_deref(),
    )
    .map_or(AnswerCheck::Accept, |value| {
        AnswerCheck::Read(value.to_owned())
    })
}

/// The entries of the answers with what was typed for a number, a day or a rank that a screen
/// without the kind drew as a text box read as the kind's own answer: the words are the one value,
/// and a rank's numbers are the options they count, in order.
fn typed_as_values(spec: &FormSpec, drawn: &Drawn, context: &Value) -> Value {
    let mut context = context.clone();
    let Some(entries) = context.get_mut("answers").and_then(Value::as_array_mut) else {
        return context;
    };
    for (question, entry) in spec.questions.iter().zip(entries) {
        let typed = matches!(
            question.check,
            Check::Number { .. } | Check::Date { .. } | Check::Rank { .. }
        ) && drawn.types.get(&question.id).map(String::as_str) == Some("text");
        let Some(entry) = entry.as_object_mut().filter(|_| typed) else {
            continue;
        };
        let empty = entry
            .get("values")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty);
        let text = match entry.get("text") {
            Some(Value::String(text)) if empty => text.clone(),
            _ => continue,
        };
        let values = match &question.check {
            Check::Rank { options } => text
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|word| !word.is_empty())
                .map(|word| {
                    let counted =
                        word.bytes().all(|b| b.is_ascii_digit()) && !word.starts_with('0');
                    word.parse::<usize>()
                        .ok()
                        .filter(|_| counted)
                        .and_then(|n| options.get(n - 1))
                        .map_or_else(String::new, Clone::clone)
                })
                .collect(),
            _ => vec![text],
        };
        entry.remove("text");
        entry.insert("values".into(), json!(values));
    }
    context
}

/// The error the model reads when the person's answers keep not fitting.
fn giving_up(form: &Form, verdict: &Verdict) -> Refusal {
    let rule = verdict.failure.rule.as_str();
    let about = match &verdict.question {
        Some(id) => {
            let words = form
                .questions
                .iter()
                .find(|q| &q.id == id)
                .map(|q| clipped(&q.question, QUOTED_QUESTION))
                .unwrap_or_default();
            format!("question \"{id}\" (\"{words}\")")
        }
        None => "the whole answer".to_owned(),
    };
    let message = format!(
        "The person's answers did not fit the form after {MAX_REFUSALS} tries, so it is not asked \
         again. The last one broke the rule `{rule}` for {about}: {}. Ask another way: in plain \
         words, with a simpler form or fewer questions.",
        verdict.failure.why
    );
    let refusal = Refusal::new(message, None, rule);
    match &verdict.question {
        Some(id) => refusal.about(id.clone()),
        None => refusal,
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

/// The `Choices` component of the interface: the root of a form with nothing above it, or the
/// form inside the `Column` that holds what a question shows.
fn root_of(ui: &Value) -> Option<&Value> {
    ui.as_array()?.iter().find_map(|message| {
        message
            .get("updateComponents")?
            .get("components")?
            .as_array()?
            .iter()
            .find(|c| c.get("component").and_then(Value::as_str) == Some("Choices"))
    })
}

fn root_of_mut(ui: &mut Value) -> Option<&mut Value> {
    ui.as_array_mut()?.iter_mut().find_map(|message| {
        message
            .get_mut("updateComponents")?
            .get_mut("components")?
            .as_array_mut()?
            .iter_mut()
            .find(|c| c.get("component").and_then(Value::as_str) == Some("Choices"))
    })
}

/// What the answer is judged by: each of the five kinds as it was drawn. A kind drawn as itself is
/// judged by the contract; a yes or no and a scale asked as radio buttons are answered in the same
/// shape (`values`), and a number, a day or a rank asked as a text box is answered in words, which
/// are judged as the kind's own value ([`typed_as_values`]). The older types are left to the model.
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
                || (matches!(kind, Kind::Confirm | Kind::Scale) && shown == Some("single"))
                || (matches!(kind, Kind::Number | Kind::Date | Kind::Rank)
                    && shown == Some("text"));
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
    // What the form shows and the screen could not draw is still said, in the same words.
    let lead = format!("{lead}{}", shown::undrawn_words(form, ui));
    let attempt = surface_of(ui)
        .and_then(|surface| ask_attempt(&surface))
        .map_or(1, |n| n + 1);
    let surface = ask_retry_surface_id(call_id, attempt);
    set_surface(&mut asked, &surface);
    carry_answers(&mut asked, drawn, context, &verdict.valid);
    let asked = fits(&mut asked, form).then_some(asked);
    let refusal = Refusal::new(format!("{said}\n{lead}"), asked, rule);
    match &verdict.question {
        Some(id) => refusal.about(id.clone()),
        None => refusal,
    }
}

/// The surface asked again is no larger than a screen reads, which the defaults carried may have
/// pushed it over: the defaults are dropped, the texts first, until it fits. `false` when it does not
/// fit with none (the form is then asked in words). A form that shows nothing is left as it is.
fn fits(asked: &mut Value, form: &Form) -> bool {
    if form.questions.iter().all(|q| q.shown.is_empty()) {
        return true;
    }
    for texts in [true, false] {
        while shown::check_size(asked).is_err() {
            let carried = root_of_mut(asked)
                .and_then(|root| root.get_mut("questions")?.as_array_mut())
                .and_then(|questions| {
                    questions.iter_mut().find(|q| {
                        let text = matches!(
                            q.get("type").and_then(Value::as_str),
                            Some("text" | "longText")
                        );
                        q.get("default").is_some() && text == texts
                    })
                });
            let Some(Value::Object(question)) = carried else {
                break;
            };
            question.remove("default");
        }
    }
    shown::check_size(asked).is_ok()
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
    let room = |list: &[&str]| {
        let distinct = list
            .iter()
            .enumerate()
            .all(|(n, value)| !list[..n].contains(value));
        let within = question
            .get("max")
            .and_then(Value::as_u64)
            .is_none_or(|max| list.len() as u64 <= max);
        distinct && within
    };
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
                && (kind == "multiple" || several)
                && room(list) =>
        {
            Some(json!(list))
        }
        _ => None,
    }
}
