//! The receiver's rules for the answer to a form: `docs/api/ui-catalog-v1.md` of
//! `vymalo/another-agentic-system`, "Receiving an answer" (catalog version 7).
//!
//! The orchestrator relays an answer and does not read it, so the agent that asked checks it again,
//! whatever the client claims to have checked. For each question of the form the first rule that
//! fails is the answer's reason, tried in the catalog's order: `unknown`, `unexpected`, `required`,
//! `too_many`, `not_offered`, `grammar`, `below_min`, `above_max`, `off_step`, `not_a_permutation`.
//!
//! The rules are pure functions of the form and the answer. A decimal is an exact integer scaled by
//! 10^6, never a float, and nothing the person gave is trimmed or repaired.

use serde_json::Value;

/// The most digits before the point of a decimal, and after it.
const INT_DIGITS: usize = 15;
const FRAC_DIGITS: usize = 6;
/// A decimal is held as an integer of millionths.
const SCALE: i128 = 1_000_000;

/// Why an answer is not used: the catalog's rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rule {
    Unknown,
    Unexpected,
    Required,
    TooMany,
    NotOffered,
    Grammar,
    BelowMin,
    AboveMax,
    OffStep,
    NotAPermutation,
}

impl Rule {
    /// The rule's name in the contract.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Unexpected => "unexpected",
            Self::Required => "required",
            Self::TooMany => "too_many",
            Self::NotOffered => "not_offered",
            Self::Grammar => "grammar",
            Self::BelowMin => "below_min",
            Self::AboveMax => "above_max",
            Self::OffStep => "off_step",
            Self::NotAPermutation => "not_a_permutation",
        }
    }
}

/// An exact decimal: millionths in an `i128`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Decimal(i128);

impl Decimal {
    /// `text` as the contract's decimal: `-?(0|[1-9][0-9]{0,14})(\.[0-9]{1,6})?`, ASCII digits only,
    /// and zero has no sign. Nothing is trimmed.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        let (negative, rest) = match bytes.first() {
            Some(b'-') => (true, &bytes[1..]),
            _ => (false, bytes),
        };
        let (int, frac) = match rest.iter().position(|&b| b == b'.') {
            Some(at) => (&rest[..at], Some(&rest[at + 1..])),
            None => (rest, None),
        };
        let digits = |part: &[u8]| part.iter().all(u8::is_ascii_digit);
        if int.is_empty()
            || int.len() > INT_DIGITS
            || !digits(int)
            || (int.len() > 1 && int[0] == b'0')
        {
            return None;
        }
        let mut value: i128 = 0;
        for &digit in int {
            value = value * 10 + i128::from(digit - b'0');
        }
        value *= SCALE;
        if let Some(frac) = frac {
            if frac.is_empty() || frac.len() > FRAC_DIGITS || !digits(frac) {
                return None;
            }
            let mut tenth = SCALE / 10;
            for &digit in frac {
                value += i128::from(digit - b'0') * tenth;
                tenth /= 10;
            }
        }
        if negative && value == 0 {
            return None;
        }
        Some(Self(if negative { -value } else { value }))
    }

    pub(crate) fn is_positive(self) -> bool {
        self.0 > 0
    }

    /// Whether `self` is `base` plus a whole number of `step`s, exactly.
    fn on_step(self, base: Self, step: Self) -> bool {
        step.is_positive() && (self.0 - base.0) % step.0 == 0
    }
}

/// `text` as a whole number of a scale: `0|[1-9][0-9]?`.
pub(crate) fn parse_whole(text: &str) -> Option<u8> {
    let bytes = text.as_bytes();
    let ok = match bytes {
        [b'0'] => true,
        [first, rest @ ..] => {
            (b'1'..=b'9').contains(first) && rest.len() <= 1 && rest.iter().all(u8::is_ascii_digit)
        }
        [] => false,
    };
    if !ok {
        return None;
    }
    text.parse().ok()
}

/// Whether `text` is a calendar day that exists: `YYYY-MM-DD`, year 0001 to 9999, ASCII digits.
pub(crate) fn is_day(text: &str) -> bool {
    let b = text.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let digits = |from: usize, to: usize| {
        b[from..to].iter().try_fold(0u32, |n, &d| {
            d.is_ascii_digit().then(|| n * 10 + u32::from(d - b'0'))
        })
    };
    let (Some(year), Some(month), Some(day)) = (digits(0, 4), digits(5, 7), digits(8, 10)) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let longest = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    year >= 1 && (1..=longest).contains(&day)
}

/// A bound as the person is told it, and as it compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bound {
    pub(crate) text: String,
    pub(crate) value: Decimal,
}

impl Bound {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        Decimal::parse(text).map(|value| Self {
            text: text.to_owned(),
            value,
        })
    }
}

/// What the answer to one question is judged by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Check {
    Confirm,
    Number {
        min: Option<Bound>,
        max: Option<Bound>,
        step: Option<Bound>,
        unit: Option<String>,
    },
    Scale {
        from: u8,
        to: u8,
    },
    Date {
        min: Option<String>,
        max: Option<String>,
    },
    Rank {
        options: Vec<String>,
    },
    /// An older type: its answer is not covered by the contract and is left to the model.
    Unchecked,
}

/// One question of a form, as far as its answer is judged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QuestionSpec {
    pub(crate) id: String,
    pub(crate) required: bool,
    pub(crate) check: Check,
}

/// The form an answer is judged against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FormSpec {
    pub(crate) free_input: bool,
    pub(crate) questions: Vec<QuestionSpec>,
}

impl FormSpec {
    /// Whether any question is one the contract judges.
    pub(crate) fn judges_anything(&self) -> bool {
        self.questions
            .iter()
            .any(|q| !matches!(q.check, Check::Unchecked))
    }
}

/// What failed, and the person's words for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub(crate) rule: Rule,
    pub(crate) why: String,
}

fn failure(rule: Rule, why: impl Into<String>) -> Failure {
    Failure {
        rule,
        why: why.into(),
    }
}

/// The text of a value that is a string, else `None`.
fn text_of(value: &Value) -> Option<&str> {
    value.as_str()
}

/// `values` against one question: `Ok`, or the first rule that fails.
pub(crate) fn check_values(spec: &QuestionSpec, values: &[Value]) -> Result<(), Failure> {
    if values.is_empty() {
        return if spec.required && !matches!(spec.check, Check::Unchecked) {
            Err(failure(Rule::Required, "this question needs an answer"))
        } else {
            Ok(())
        };
    }
    let single = !matches!(spec.check, Check::Rank { .. } | Check::Unchecked);
    if single && values.len() > 1 {
        return Err(failure(Rule::TooMany, "only one value is allowed"));
    }
    match &spec.check {
        Check::Unchecked => Ok(()),
        Check::Confirm => match text_of(&values[0]) {
            Some("yes" | "no") => Ok(()),
            _ => Err(failure(
                Rule::NotOffered,
                "the answer is neither yes nor no",
            )),
        },
        Check::Number {
            min,
            max,
            step,
            unit,
        } => check_number(
            &values[0],
            min.as_ref(),
            max.as_ref(),
            step.as_ref(),
            unit.as_deref(),
        ),
        Check::Scale { from, to } => {
            let Some(text) = text_of(&values[0]) else {
                return Err(failure(Rule::Grammar, "the answer is not a whole number"));
            };
            let Some(point) = parse_whole(text) else {
                return Err(failure(
                    Rule::Grammar,
                    "the answer is not a whole number written with digits, like 7",
                ));
            };
            if point < *from {
                Err(failure(
                    Rule::BelowMin,
                    format!("{point} is below the first point, {from}"),
                ))
            } else if point > *to {
                Err(failure(
                    Rule::AboveMax,
                    format!("{point} is above the last point, {to}"),
                ))
            } else {
                Ok(())
            }
        }
        Check::Date { min, max } => {
            let Some(text) = text_of(&values[0]).filter(|t| is_day(t)) else {
                return Err(failure(
                    Rule::Grammar,
                    "the answer is not a day written YYYY-MM-DD, like 2026-12-15",
                ));
            };
            if let Some(min) = min.as_deref().filter(|min| text < *min) {
                Err(failure(
                    Rule::BelowMin,
                    format!("{text} is before the first day, {min}"),
                ))
            } else if let Some(max) = max.as_deref().filter(|max| text > *max) {
                Err(failure(
                    Rule::AboveMax,
                    format!("{text} is after the last day, {max}"),
                ))
            } else {
                Ok(())
            }
        }
        Check::Rank { options } => {
            let mut given: Vec<&str> = Vec::with_capacity(values.len());
            for value in values {
                match text_of(value).filter(|v| options.iter().any(|o| o == v)) {
                    Some(value) => given.push(value),
                    None => {
                        return Err(failure(
                            Rule::NotOffered,
                            "a value is not one of the options",
                        ));
                    }
                }
            }
            let mut sorted = given.clone();
            sorted.sort_unstable();
            sorted.dedup();
            if given.len() == options.len() && sorted.len() == options.len() {
                Ok(())
            } else {
                Err(failure(
                    Rule::NotAPermutation,
                    "every option has to appear once, in your order",
                ))
            }
        }
    }
}

fn check_number(
    value: &Value,
    min: Option<&Bound>,
    max: Option<&Bound>,
    step: Option<&Bound>,
    unit: Option<&str>,
) -> Result<(), Failure> {
    let Some(text) = text_of(value) else {
        return Err(failure(Rule::Grammar, "the answer is not a number"));
    };
    let Some(number) = Decimal::parse(text) else {
        return Err(failure(
            Rule::Grammar,
            "the answer is not a number written with digits and a point, like 12.5",
        ));
    };
    let with_unit = |bound: &Bound| match unit {
        Some(unit) => format!("{} {unit}", bound.text),
        None => bound.text.clone(),
    };
    if let Some(min) = min.filter(|min| number < min.value) {
        return Err(failure(
            Rule::BelowMin,
            format!("{text} is below the least, {}", with_unit(min)),
        ));
    }
    if let Some(max) = max.filter(|max| number > max.value) {
        return Err(failure(
            Rule::AboveMax,
            format!("{text} is above the most, {}", with_unit(max)),
        ));
    }
    if let Some(step) = step {
        let base = min.map_or(Decimal(0), |min| min.value);
        if !number.on_step(base, step.value) {
            let from = min.map_or("0", |min| min.text.as_str());
            return Err(failure(
                Rule::OffStep,
                format!("{text} is not {from} plus a whole number of {}", step.text),
            ));
        }
    }
    Ok(())
}

/// An answer that is not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    /// The question the first failing rule is about, or `None` for the whole answer.
    pub(crate) question: Option<String>,
    pub(crate) failure: Failure,
    /// Per question, in order: whether its own answer passed every rule. Empty when the whole answer
    /// failed, because then no entry can be trusted to belong to its question.
    pub(crate) valid: Vec<bool>,
}

fn whole(rule: Rule, why: &str) -> Verdict {
    Verdict {
        question: None,
        failure: failure(rule, why),
        valid: Vec::new(),
    }
}

/// The context of the action that answered `form`: `Ok`, or the first failure and which questions
/// were answered well.
pub(crate) fn check_answer(form: &FormSpec, context: &Value) -> Result<(), Verdict> {
    let Some(answers) = context.get("answers").and_then(Value::as_array) else {
        return Err(whole(Rule::Unknown, "the answer has no list of answers"));
    };
    let in_order = answers.len() == form.questions.len()
        && answers
            .iter()
            .zip(&form.questions)
            .all(|(answer, question)| {
                answer.get("id").and_then(Value::as_str) == Some(question.id.as_str())
            });
    if !in_order {
        return Err(whole(
            Rule::Unknown,
            "the answers are not the form's questions, once each, in order",
        ));
    }
    if let Some(members) = context.as_object()
        && members.keys().any(|key| key != "answers" && key != "note")
    {
        return Err(whole(
            Rule::Unexpected,
            "the answer has a member the form does not take",
        ));
    }
    match context.get("note") {
        None | Some(Value::Null) => {}
        Some(Value::String(_)) if form.free_input => {}
        Some(_) => {
            return Err(whole(
                Rule::Unexpected,
                "the answer has a note, and the form has no box for one",
            ));
        }
    }
    let mut first: Option<(String, Failure)> = None;
    let mut valid = Vec::with_capacity(form.questions.len());
    for (question, answer) in form.questions.iter().zip(answers) {
        let outcome = check_entry(question, answer);
        valid.push(outcome.is_ok());
        if let (Err(found), None) = (outcome, &first) {
            first = Some((question.id.clone(), found));
        }
    }
    match first {
        None => Ok(()),
        Some((question, failure)) => Err(Verdict {
            question: Some(question),
            failure,
            valid,
        }),
    }
}

/// One entry of the answers against its question.
fn check_entry(question: &QuestionSpec, answer: &Value) -> Result<(), Failure> {
    let Some(values) = answer.get("values").and_then(Value::as_array) else {
        return Err(failure(Rule::Unknown, "the answer is not a list of values"));
    };
    if matches!(question.check, Check::Unchecked) {
        return Ok(());
    }
    if answer
        .as_object()
        .is_some_and(|members| members.keys().any(|key| key != "id" && key != "values"))
    {
        return Err(failure(
            Rule::Unexpected,
            "this kind of question takes `values` and nothing else",
        ));
    }
    check_values(question, values)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use serde_json::json;

    #[test]
    fn a_decimal_is_the_contracts_grammar_and_compares_exactly() {
        for good in [
            "0",
            "0.0",
            "0.000000",
            "-0.000001",
            "12.50",
            "123456789012345.123456",
        ] {
            assert!(Decimal::parse(good).is_some(), "{good}");
        }
        for bad in [
            "",
            "-",
            "-0",
            "-0.0",
            "-0.000000",
            "05",
            "+1",
            "1.",
            ".5",
            "1e3",
            "1,5",
            " 5",
            "5 ",
            "5\n",
            "1 000",
            "٣",
            "５",
            "NaN",
            "1234567890123456",
            "0.1234567",
        ] {
            assert!(Decimal::parse(bad).is_none(), "{bad:?}");
        }
        let d = |t: &str| Decimal::parse(t).unwrap();
        assert!(d("1000.5") > d("1000"));
        assert_eq!(d("2.50"), d("2.5"));
        // 0.1 + 0.2 is 0.3 as exact decimals, and a float would say otherwise.
        assert_eq!(d("0.1").0 + d("0.2").0, d("0.3").0);
    }

    #[test]
    fn on_the_step_counts_from_the_least_or_zero_and_may_be_negative() {
        let d = |t: &str| Decimal::parse(t).unwrap();
        assert!(d("250.5").on_step(d("0"), d("0.5")));
        assert!(!d("250.25").on_step(d("0"), d("0.5")));
        assert!(d("-2.5").on_step(d("-5"), d("2.5")));
        assert!(!d("2").on_step(d("1"), d("2")));
    }

    #[test]
    fn a_day_is_a_real_day_of_the_gregorian_calendar() {
        for good in ["2028-02-29", "2000-02-29", "0001-01-01", "9999-12-31"] {
            assert!(is_day(good), "{good}");
        }
        for bad in [
            "2027-02-29",
            "1900-02-29",
            "2100-02-29",
            "2026-04-31",
            "0000-01-01",
            "2026-00-10",
            "2026-12-00",
            "2026-13-01",
            "2026-12-1",
            "２０２６-12-15",
            "2026-12-15T00:00:00Z",
        ] {
            assert!(!is_day(bad), "{bad}");
        }
    }

    #[test]
    fn a_whole_number_of_a_scale_has_no_leading_zero_and_no_point() {
        assert_eq!(parse_whole("0"), Some(0));
        assert_eq!(parse_whole("10"), Some(10));
        for bad in ["", "07", "7.0", "-1", "7 ", "٧", "100"] {
            assert_eq!(parse_whole(bad), None, "{bad:?}");
        }
    }

    fn rank(required: bool) -> QuestionSpec {
        QuestionSpec {
            id: "r".into(),
            required,
            check: Check::Rank {
                options: vec!["a".into(), "b".into(), "c".into()],
            },
        }
    }

    #[test]
    fn a_rank_that_names_a_stranger_is_not_offered_before_it_is_not_a_permutation() {
        let spec = rank(true);
        let rule = |values: Value| {
            check_values(&spec, values.as_array().unwrap())
                .err()
                .map(|f| f.rule)
        };
        assert_eq!(rule(json!(["a", "a", "d"])), Some(Rule::NotOffered));
        assert_eq!(rule(json!(["a", "a", "b"])), Some(Rule::NotAPermutation));
        assert_eq!(rule(json!(["c", "a", "b"])), None);
    }

    #[test]
    fn a_failure_in_the_middle_still_reports_which_questions_were_answered_well() {
        let form = FormSpec {
            free_input: false,
            questions: vec![
                QuestionSpec {
                    id: "ok".into(),
                    required: true,
                    check: Check::Confirm,
                },
                QuestionSpec {
                    id: "n".into(),
                    required: true,
                    check: Check::Number {
                        min: None,
                        max: Bound::parse("10"),
                        step: None,
                        unit: Some("kg".into()),
                    },
                },
            ],
        };
        let context =
            json!({"answers": [{"id": "ok", "values": ["yes"]}, {"id": "n", "values": ["11"]}]});
        let verdict = check_answer(&form, &context).unwrap_err();
        assert_eq!(verdict.question.as_deref(), Some("n"));
        assert_eq!(verdict.failure.rule, Rule::AboveMax);
        assert_eq!(verdict.failure.why, "11 is above the most, 10 kg");
        assert_eq!(verdict.valid, [true, false]);
    }
}
