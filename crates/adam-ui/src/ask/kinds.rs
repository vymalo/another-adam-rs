//! The five kinds of question that catalog version 7 adds to `Choices`: `confirm`, `number`, `scale`,
//! `date` and `rank`. What the model may say about each, how it is checked before the form is
//! drawn, how it is drawn, how it is worded for a screen that does not have it (the nearest form of
//! version 6, or words), and how it is judged when answered.
//!
//! What a screen has is read from its `Choices` schema, one kind at a time ([`Kinds::probe`]), not
//! from the version number: the catalog says the schema is the better test, and a later version may
//! add more.

use adam_a2a_runtime::is_format_or_control;
use serde_json::{Map, Value, json};

use super::answer::{Bound, Check, Decimal, QuestionSpec, check_values, is_day, parse_whole};
use super::{
    ChoiceOption, DefaultArg, Form, Kind, MAX_LABEL, MAX_OPTIONS, MIN_OPTIONS, Question, clipped,
    resolve_defaults,
};
use crate::catalog::Catalog;

/// The longest `yesLabel` and `noLabel`.
const MAX_YES_NO: usize = 40;
/// The longest `unit`.
const MAX_UNIT: usize = 20;
/// The longest `fromLabel` and `toLabel`.
const MAX_END_LABEL: usize = 60;
/// The most points of a scale: `to` is at most 10 and `from` at least 0.
const MAX_TO: i64 = 10;
/// The longest `help` of a question: the component's limit.
const MAX_HELP: usize = 300;
/// The longest a value quoted back to the model is.
const SHOWN: usize = 40;

/// A number the model may write as a JSON number or as a string.
#[derive(Debug, Clone)]
pub(super) enum Scalar {
    Text(String),
    Number(serde_json::Number),
}

impl Scalar {
    /// What the model gave for `member` of question `id`, or what it got wrong, naming the member.
    pub(super) fn of(id: &str, member: &str, given: Value) -> Result<Self, String> {
        match given {
            Value::String(text) => Ok(Self::Text(text)),
            Value::Number(number) => Ok(Self::Number(number)),
            other => Err(format!(
                "`{member}` of question `{id}` is {}, which is neither a string nor a number: \
                 write it as a string like \"12.5\" or a number",
                shown(&other.to_string())
            )),
        }
    }
}

/// What the model said about the members of the five kinds, as it said it.
#[derive(Debug, Default)]
pub(super) struct ExtRaw {
    pub(super) yes_label: Option<String>,
    pub(super) no_label: Option<String>,
    pub(super) min_value: Option<Scalar>,
    pub(super) max_value: Option<Scalar>,
    pub(super) step: Option<Scalar>,
    pub(super) unit: Option<String>,
    pub(super) from: Option<Scalar>,
    pub(super) to: Option<Scalar>,
    pub(super) from_label: Option<String>,
    pub(super) to_label: Option<String>,
    pub(super) min_date: Option<String>,
    pub(super) max_date: Option<String>,
}

/// The members of the five kinds, checked: decimals and days as the grammar writes them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Ext {
    pub(super) yes_label: Option<String>,
    pub(super) no_label: Option<String>,
    pub(super) min_value: Option<String>,
    pub(super) max_value: Option<String>,
    pub(super) step: Option<String>,
    pub(super) unit: Option<String>,
    pub(super) from: Option<u8>,
    pub(super) to: Option<u8>,
    pub(super) from_label: Option<String>,
    pub(super) to_label: Option<String>,
    pub(super) min_date: Option<String>,
    pub(super) max_date: Option<String>,
}

impl Kind {
    /// One of the five kinds of version 7.
    pub(super) fn is_extended(self) -> bool {
        matches!(
            self,
            Self::Confirm | Self::Number | Self::Scale | Self::Date | Self::Rank
        )
    }
}

impl ExtRaw {
    /// The members the model gave, with the kind each belongs to.
    fn present(&self) -> Vec<(&'static str, Kind)> {
        let mut found = Vec::new();
        let mut note = |given: bool, member: &'static str, kind: Kind| {
            if given {
                found.push((member, kind));
            }
        };
        note(self.yes_label.is_some(), "yesLabel", Kind::Confirm);
        note(self.no_label.is_some(), "noLabel", Kind::Confirm);
        note(self.min_value.is_some(), "minValue", Kind::Number);
        note(self.max_value.is_some(), "maxValue", Kind::Number);
        note(self.step.is_some(), "step", Kind::Number);
        note(self.unit.is_some(), "unit", Kind::Number);
        note(self.from.is_some(), "from", Kind::Scale);
        note(self.to.is_some(), "to", Kind::Scale);
        note(self.from_label.is_some(), "fromLabel", Kind::Scale);
        note(self.to_label.is_some(), "toLabel", Kind::Scale);
        note(self.min_date.is_some(), "minDate", Kind::Date);
        note(self.max_date.is_some(), "maxDate", Kind::Date);
        found
    }

    /// A member that belongs to another kind is refused, as the catalog refuses it.
    pub(super) fn reject_foreign(&self, id: &str, kind: Kind) -> Result<(), String> {
        match self.present().into_iter().find(|(_, owner)| *owner != kind) {
            None => Ok(()),
            Some((member, owner)) => Err(format!(
                "question `{id}` is type \"{}\" but has `{member}`, which belongs to type \"{}\"",
                kind.as_str(),
                owner.as_str()
            )),
        }
    }
}

/// `text` as it is quoted back to the model: cut, and in quotes.
fn shown(text: &str) -> String {
    format!("{:?}", clipped(text, SHOWN))
}

/// A decimal the model gave as a string or a JSON number, in the grammar of the catalog.
fn decimal_text(id: &str, member: &str, given: Scalar) -> Result<String, String> {
    let text = match given {
        Scalar::Text(text) => text.trim().to_owned(),
        Scalar::Number(number) => number.to_string(),
    };
    if Decimal::parse(&text).is_some() {
        return Ok(text);
    }
    Err(format!(
        "`{member}` of question `{id}` is {}, which is not a decimal: write digits and a point, like \"12.5\" \
         (no plus sign, no exponent, no comma, at most 15 digits before the point and 6 after; zero has no sign)",
        shown(&text)
    ))
}

/// A whole number the model gave as a string or a JSON number: a string in the grammar of a scale's
/// points (`0` or `7`, never `07` or `+1`).
fn whole_of(id: &str, member: &str, given: Scalar) -> Result<i64, String> {
    let parsed = match &given {
        Scalar::Text(text) => parse_whole(text.trim()).map(i64::from),
        Scalar::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|f| f.fract() == 0.0 && f.abs() < 1e9)
                // The test above makes this cast exact.
                .map(|f| f as i64)
        }),
    };
    parsed.ok_or_else(|| format!("`{member}` of question `{id}` is not a whole number"))
}

/// `text` has nothing the person cannot see: no hidden, control or bidirectional character (a line
/// break and a tab are allowed when `breaks` says the member may have lines).
pub(super) fn plain(id: &str, member: &str, text: &str, breaks: bool) -> Result<(), String> {
    match text
        .chars()
        .find(|c| !(breaks && matches!(c, '\t' | '\n' | '\r')) && is_format_or_control(*c))
    {
        Some(hidden) => Err(format!(
            "`{member}` of question `{id}` has a character the person cannot see (U+{:04X}): write plain words",
            u32::from(hidden)
        )),
        None => Ok(()),
    }
}

/// A label of 1 to `most` characters, or nothing.
fn label_of(
    id: &str,
    member: &str,
    given: Option<String>,
    most: usize,
) -> Result<Option<String>, String> {
    let Some(text) = given else { return Ok(None) };
    let text = text.trim().to_owned();
    if text.is_empty() || text.chars().count() > most {
        return Err(format!(
            "`{member}` of question `{id}` has to be 1 to {most} characters"
        ));
    }
    plain(id, member, &text, false)?;
    Ok(Some(text))
}

/// The question of one of the five kinds, with its members checked: what the model got wrong comes
/// back in words it can act on, before the form is drawn.
pub(super) fn finish(
    mut out: Question,
    raw: ExtRaw,
    default: Option<DefaultArg>,
    multiple: Option<bool>,
) -> Result<Question, String> {
    let id = out.id.clone();
    let kind = out.kind();
    let name = kind.as_str();
    raw.reject_foreign(&id, kind)?;
    if multiple.is_some() || out.allow_other.is_some() || out.min.is_some() || out.max.is_some() {
        return Err(format!(
            "question `{id}` is type \"{name}\": it takes no `multiple`, `allowOther`, `min` or `max`"
        ));
    }
    if kind != Kind::Rank && !out.options.is_empty() {
        return Err(format!(
            "question `{id}` is type \"{name}\": it takes no options (only type \"rank\" does)"
        ));
    }
    let not_that = |what: &str| {
        format!(
            "the default of question `{id}` is {what}, which a \"{name}\" question does not take"
        )
    };
    let mut ext = Ext::default();
    match kind {
        Kind::Confirm => {
            ext.yes_label = label_of(&id, "yesLabel", raw.yes_label, MAX_YES_NO)?;
            ext.no_label = label_of(&id, "noLabel", raw.no_label, MAX_YES_NO)?;
            match default {
                None => {}
                Some(DefaultArg::One(word)) => {
                    let word = word.trim();
                    let Some(safe) =
                        confirm_answer(word, ext.yes_label.as_deref(), ext.no_label.as_deref())
                    else {
                        return Err(format!(
                            "the default of question `{id}` is {}: use \"yes\" or \"no\", the safe answer",
                            shown(word)
                        ));
                    };
                    out.default_text = Some(safe.to_owned());
                }
                Some(DefaultArg::Many(_)) => return Err(not_that("a list")),
                Some(DefaultArg::Number(_)) => return Err(not_that("a number")),
            }
        }
        Kind::Number => {
            let decimal = |member, given: Option<Scalar>| {
                given
                    .map(|given| decimal_text(&id, member, given))
                    .transpose()
            };
            ext.min_value = decimal("minValue", raw.min_value)?;
            ext.max_value = decimal("maxValue", raw.max_value)?;
            ext.step = decimal("step", raw.step)?;
            ext.unit = label_of(&id, "unit", raw.unit, MAX_UNIT)?;
            let parsed = |text: &Option<String>| text.as_deref().and_then(Decimal::parse);
            if let (Some(min), Some(max)) = (parsed(&ext.min_value), parsed(&ext.max_value))
                && min > max
            {
                return Err(format!(
                    "the `minValue` of question `{id}` is above its `maxValue`"
                ));
            }
            if parsed(&ext.step).is_some_and(|step| !step.is_positive()) {
                return Err(format!(
                    "the `step` of question `{id}` has to be above zero"
                ));
            }
            out.default_text = match default {
                None => None,
                Some(DefaultArg::One(text)) => {
                    Some(decimal_text(&id, "default", Scalar::Text(text))?)
                }
                Some(DefaultArg::Number(number)) => {
                    Some(decimal_text(&id, "default", Scalar::Number(number))?)
                }
                Some(DefaultArg::Many(_)) => return Err(not_that("a list")),
            };
        }
        Kind::Scale => {
            let from = raw
                .from
                .map(|given| whole_of(&id, "from", given))
                .transpose()?;
            let to = raw.to.map(|given| whole_of(&id, "to", given)).transpose()?;
            if from.is_some_and(|from| !(0..=1).contains(&from)) {
                return Err(format!("the `from` of question `{id}` is 0 or 1"));
            }
            let Some(to) = to else {
                return Err(format!(
                    "question `{id}` is type \"scale\": `to` is required (2 to 10, the last point)"
                ));
            };
            if !(2..=MAX_TO).contains(&to) {
                return Err(format!("the `to` of question `{id}` is 2 to 10"));
            }
            if to <= from.unwrap_or(1) {
                return Err(format!(
                    "the `to` of question `{id}` is not above its `from`"
                ));
            }
            ext.from = from.and_then(|from| u8::try_from(from).ok());
            ext.to = u8::try_from(to).ok();
            ext.from_label = label_of(&id, "fromLabel", raw.from_label, MAX_END_LABEL)?;
            ext.to_label = label_of(&id, "toLabel", raw.to_label, MAX_END_LABEL)?;
            out.default_text = match default {
                None => None,
                Some(DefaultArg::One(text)) => Some(text.trim().to_owned()),
                Some(DefaultArg::Number(number)) => Some(number.to_string()),
                Some(DefaultArg::Many(_)) => return Err(not_that("a list")),
            };
        }
        Kind::Date => {
            for (member, given) in [("minDate", &raw.min_date), ("maxDate", &raw.max_date)] {
                if let Some(day) = given.as_deref().map(str::trim)
                    && !is_day(day)
                {
                    return Err(format!(
                        "`{member}` of question `{id}` is {}, which is not a day: write YYYY-MM-DD, a day that exists",
                        shown(day)
                    ));
                }
            }
            ext.min_date = raw.min_date.map(|d| d.trim().to_owned());
            ext.max_date = raw.max_date.map(|d| d.trim().to_owned());
            if let (Some(min), Some(max)) = (&ext.min_date, &ext.max_date)
                && min > max
            {
                return Err(format!(
                    "the `minDate` of question `{id}` is after its `maxDate`"
                ));
            }
            out.default_text = match default {
                None => None,
                Some(DefaultArg::One(text)) => Some(text.trim().to_owned()),
                Some(DefaultArg::Many(_)) => return Err(not_that("a list")),
                Some(DefaultArg::Number(_)) => return Err(not_that("a number")),
            };
        }
        Kind::Rank => {
            if out.options.len() < MIN_OPTIONS || out.options.len() > MAX_OPTIONS {
                return Err(format!(
                    "question `{id}` has {} options; a \"rank\" question needs {MIN_OPTIONS} to {MAX_OPTIONS}",
                    out.options.len()
                ));
            }
            if let Some(option) = out.options.iter().find(|o| o.image.is_some()) {
                return Err(format!(
                    "option `{}` of question `{id}` has an image, which a \"rank\" question does not draw",
                    option.value
                ));
            }
            match default {
                None => {}
                Some(DefaultArg::Many(list)) => {
                    out.defaults = resolve_defaults(&id, DefaultArg::Many(list), &out.options)?;
                }
                Some(_) => {
                    return Err(format!(
                        "the default of question `{id}` is a list of every option once, in the order the list starts in"
                    ));
                }
            }
        }
        _ => {}
    }
    out.ext = ext;
    if let Some(problem) = default_problem(&out) {
        return Err(format!(
            "the default of question `{id}` is not an answer the question accepts: {problem}"
        ));
    }
    Ok(out)
}

/// The answer `word` is to a yes or no whose buttons say `yes_label` and `no_label` ("Yes" and "No"
/// when absent): `yes` or `no`, or one of the two labels, in any case. `None` when it is neither,
/// or when it says both.
pub(super) fn confirm_answer(
    word: &str,
    yes_label: Option<&str>,
    no_label: Option<&str>,
) -> Option<&'static str> {
    let word = word.to_lowercase();
    let says = |label: &str| label.to_lowercase() == word;
    let yes = word == "yes" || says(yes_label.unwrap_or("Yes"));
    let no = word == "no" || says(no_label.unwrap_or("No"));
    match (yes, no) {
        (true, false) => Some("yes"),
        (false, true) => Some("no"),
        _ => None,
    }
}

/// What is wrong with the question's default as an answer, if it has one.
fn default_problem(question: &Question) -> Option<String> {
    let values: Vec<Value> = match question.kind() {
        Kind::Rank => question.defaults.iter().map(|v| json!(v)).collect(),
        _ => question.default_text.iter().map(|v| json!(v)).collect(),
    };
    if values.is_empty() {
        return None;
    }
    check_values(&spec_of(question), &values)
        .err()
        .map(|failure| format!("{} ({})", failure.why, failure.rule.as_str()))
}

/// What the answer to `question` is judged by: the five kinds as the contract judges them, and
/// every older type as left to the model.
pub(super) fn spec_of(question: &Question) -> QuestionSpec {
    let ext = &question.ext;
    let bound = |text: &Option<String>| text.as_deref().and_then(Bound::parse);
    let check = match question.kind() {
        Kind::Confirm => Check::Confirm,
        Kind::Number => Check::Number {
            min: bound(&ext.min_value),
            max: bound(&ext.max_value),
            step: bound(&ext.step),
            unit: ext.unit.clone(),
        },
        Kind::Scale => Check::Scale {
            from: ext.from.unwrap_or(1),
            to: ext.to.unwrap_or(1),
        },
        Kind::Date => Check::Date {
            min: ext.min_date.clone(),
            max: ext.max_date.clone(),
        },
        Kind::Rank => Check::Rank {
            options: question.options.iter().map(|o| o.value.clone()).collect(),
        },
        _ => Check::Unchecked,
    };
    QuestionSpec {
        id: question.id.clone(),
        required: question.required.unwrap_or(true),
        check,
    }
}

/// The kinds of version 7 that a screen's `Choices` takes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Kinds {
    confirm: bool,
    number: bool,
    scale: bool,
    date: bool,
    rank: bool,
}

impl Kinds {
    /// Which of the five `catalog`'s `Choices` takes: each is asked of the schema with a question
    /// that has every member of its kind, because a screen that lists a type but not a member is as
    /// good as one that does not list it.
    pub(super) fn probe(catalog: &Catalog) -> Self {
        let takes = |question: Value| {
            let mut question = question;
            question["id"] = json!("probe");
            question["question"] = json!("x");
            catalog
                .validate(&json!({"id": "probe", "component": "Choices", "questions": [question]}))
                .is_ok()
        };
        Self {
            confirm: takes(
                json!({"type": "confirm", "yesLabel": "y", "noLabel": "n", "default": "no"}),
            ),
            number: takes(
                json!({"type": "number", "minValue": "0", "maxValue": "1", "step": "1", "unit": "u", "default": "0"}),
            ),
            scale: takes(
                json!({"type": "scale", "from": 0, "to": 5, "fromLabel": "a", "toLabel": "b", "default": "1"}),
            ),
            date: takes(
                json!({"type": "date", "minDate": "2026-01-01", "maxDate": "2026-12-31", "default": "2026-06-01"}),
            ),
            rank: takes(
                json!({"type": "rank", "options": [{"value": "a", "label": "A"}, {"value": "b", "label": "B"}], "default": ["a", "b"]}),
            ),
        }
    }

    fn has(self, kind: Kind) -> bool {
        match kind {
            Kind::Confirm => self.confirm,
            Kind::Number => self.number,
            Kind::Scale => self.scale,
            Kind::Date => self.date,
            Kind::Rank => self.rank,
            _ => true,
        }
    }
}

/// The form as a screen that has `kinds` can draw it: a kind it lacks becomes the nearest form of
/// version 6. `None` when that cannot be done (a rank whose options do not fit in a help line): the
/// form then goes as words.
pub(super) fn lowered(form: &Form, kinds: Kinds) -> Option<Form> {
    let questions = form
        .questions
        .iter()
        .map(|q| {
            if q.kind().is_extended() && !kinds.has(q.kind()) {
                lower_question(q)
            } else {
                Some(q.clone())
            }
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Form {
        questions,
        ..form.clone()
    })
}

/// A question of one of the five kinds as version 6 has it.
fn lower_question(q: &Question) -> Option<Question> {
    let mut lowered = q.clone();
    lowered.ext = Ext::default();
    lowered.default_text = None;
    lowered.defaults = Vec::new();
    match q.kind() {
        Kind::Confirm => {
            let option = |value: &str, label: Option<&String>, fallback: &str| ChoiceOption {
                value: value.to_owned(),
                label: label.map_or_else(|| fallback.to_owned(), Clone::clone),
                description: None,
                image: None,
                icon: None,
                recommended: false,
            };
            lowered.declared = Some(Kind::Single);
            lowered.options = vec![
                option("yes", q.ext.yes_label.as_ref(), "Yes"),
                option("no", q.ext.no_label.as_ref(), "No"),
            ];
            lowered.defaults = q.default_text.iter().cloned().collect();
        }
        Kind::Scale => {
            let (from, to) = (q.ext.from.unwrap_or(1), q.ext.to.unwrap_or(1));
            lowered.declared = Some(Kind::Single);
            lowered.options = (from..=to)
                .map(|point| {
                    let end = if point == from {
                        q.ext.from_label.as_deref()
                    } else if point == to {
                        q.ext.to_label.as_deref()
                    } else {
                        None
                    };
                    ChoiceOption {
                        value: point.to_string(),
                        label: match end {
                            Some(words) => clipped(&format!("{point} - {words}"), MAX_LABEL),
                            None => point.to_string(),
                        },
                        description: None,
                        image: None,
                        icon: None,
                        recommended: false,
                    }
                })
                .collect();
            lowered.defaults = q.default_text.iter().cloned().collect();
        }
        Kind::Number | Kind::Date | Kind::Rank => {
            lowered.declared = Some(Kind::Text);
            lowered.options = Vec::new();
            lowered.default_text = q.default_text.clone();
            let hint = hint_of(q)?;
            let help = match &q.help {
                Some(help) => format!("{hint} {help}"),
                None => hint,
            };
            lowered.help = Some(clipped(&help, MAX_HELP));
        }
        _ => {}
    }
    Some(lowered)
}

/// The sentence a typed question carries in its `help` when the screen has no control for the kind,
/// or `None` when it does not fit in one.
fn hint_of(q: &Question) -> Option<String> {
    let phrase = phrase(q);
    let mut chars = phrase.chars();
    let mut hint: String = chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default();
    hint.push('.');
    if q.kind() == Kind::Rank {
        for (n, option) in q.options.iter().enumerate() {
            hint.push_str(&format!(" {}) {}", n + 1, option.label));
        }
        if hint.chars().count() > MAX_HELP {
            return None;
        }
    }
    Some(hint)
}

/// What the person is asked for, in words (without brackets): the text form of the kind, and the
/// hint of a typed question that stands for it.
pub(super) fn phrase(q: &Question) -> String {
    let ext = &q.ext;
    match q.kind() {
        Kind::Confirm => {
            let yes = ext.yes_label.as_deref().unwrap_or("Yes");
            let no = ext.no_label.as_deref().unwrap_or("No");
            match q.default_text.as_deref() {
                Some(safe) => format!("yes: \"{yes}\" / no: \"{no}\"; the safe answer is {safe}"),
                None => format!("yes: \"{yes}\" / no: \"{no}\""),
            }
        }
        Kind::Number => {
            let unit = ext.unit.as_deref();
            let range = match (&ext.min_value, &ext.max_value) {
                (Some(min), Some(max)) => format!(" from {min} to {max}"),
                (Some(min), None) => format!(" of at least {min}"),
                (None, Some(max)) => format!(" of at most {max}"),
                (None, None) => String::new(),
            };
            let unit_part = match (unit, range.is_empty()) {
                (Some(unit), false) => format!(" {unit}"),
                (Some(unit), true) => format!(" in {unit}"),
                (None, _) => String::new(),
            };
            let step = ext
                .step
                .as_deref()
                .map(|step| format!(", in steps of {step}"))
                .unwrap_or_default();
            format!("a number{range}{unit_part}{step}")
        }
        Kind::Scale => {
            let (from, to) = (ext.from.unwrap_or(1), ext.to.unwrap_or(1));
            let end = |label: &Option<String>| {
                label
                    .as_deref()
                    .map(|words| format!(" \"{words}\""))
                    .unwrap_or_default()
            };
            format!(
                "a whole number from {from}{} to {to}{}",
                end(&ext.from_label),
                end(&ext.to_label)
            )
        }
        Kind::Date => {
            let range = match (&ext.min_date, &ext.max_date) {
                (Some(min), Some(max)) => format!(", from {min} to {max}"),
                (Some(min), None) => format!(", on or after {min}"),
                (None, Some(max)) => format!(", on or before {max}"),
                (None, None) => String::new(),
            };
            format!("a day, YYYY-MM-DD{range}")
        }
        Kind::Rank => {
            "put the options in order, the best first: answer with their numbers in order, like 2, 1, 3"
                .to_owned()
        }
        _ => String::new(),
    }
}

/// The members of a kind in the `Choices` component, beside the ones every question has.
pub(super) fn native_members(q: &Question, question: &mut Map<String, Value>) {
    let ext = &q.ext;
    let mut put = |member: &str, value: Option<Value>| {
        if let Some(value) = value {
            question.insert(member.into(), value);
        }
    };
    put("yesLabel", ext.yes_label.as_ref().map(|v| json!(v)));
    put("noLabel", ext.no_label.as_ref().map(|v| json!(v)));
    put("minValue", ext.min_value.as_ref().map(|v| json!(v)));
    put("maxValue", ext.max_value.as_ref().map(|v| json!(v)));
    put("step", ext.step.as_ref().map(|v| json!(v)));
    put("unit", ext.unit.as_ref().map(|v| json!(v)));
    put("from", ext.from.map(|v| json!(v)));
    put("to", ext.to.map(|v| json!(v)));
    put("fromLabel", ext.from_label.as_ref().map(|v| json!(v)));
    put("toLabel", ext.to_label.as_ref().map(|v| json!(v)));
    put("minDate", ext.min_date.as_ref().map(|v| json!(v)));
    put("maxDate", ext.max_date.as_ref().map(|v| json!(v)));
}
