//! The conformance vectors of catalog version 7 (`docs/api/ui-catalog-v1.md` of
//! `vymalo/another-agentic-system`, "Receiving an answer"), vendored in
//! `tests/fixtures/choices-v7-vectors.json`: every case is run through the agent's own reading of a
//! question and of an answer.
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests assert by unwrapping

use serde_json::{Value, json};

use super::answer::{Check, FormSpec, check_answer, check_values};
use super::{Args, Form, kinds, normalize};

const VECTORS: &str = include_str!("../../tests/fixtures/choices-v7-vectors.json");

/// The number of cases of `vectors` the contract page says it holds.
const CASES: usize = 133;

fn document() -> Value {
    serde_json::from_str(VECTORS).unwrap()
}

/// The form of a call that asks `questions`, free box or not.
fn form_of(questions: &[Value], free_input: bool) -> Result<Form, String> {
    let args: Args = serde_json::from_value(json!({
        "question": "q", "freeInput": free_input, "choices": questions
    }))
    .map_err(|e| e.to_string())?;
    normalize(args)
}

fn spec_of(form: &Form, free_input: bool) -> FormSpec {
    FormSpec {
        free_input,
        questions: form.questions.iter().map(kinds::spec_of).collect(),
    }
}

#[test]
fn every_case_of_the_vectors_gets_the_rule_the_contract_says() {
    let document = document();
    let blocks = document["vectors"].as_array().unwrap();
    let mut run = 0;
    for block in blocks {
        let form = form_of(&[block["question"].clone()], false)
            .unwrap_or_else(|e| panic!("{}: {e}", block["question"]));
        let spec = kinds::spec_of(&form.questions[0]);
        assert!(!matches!(spec.check, Check::Unchecked));
        for case in block["cases"].as_array().unwrap() {
            let values = case[0].as_array().unwrap();
            let want = case[1].as_str();
            let got = check_values(&spec, values).err().map(|f| f.rule.as_str());
            assert_eq!(got, want, "{} answered {values:?}", block["question"]["id"]);
            run += 1;
        }
    }
    assert_eq!(
        run, CASES,
        "a case was deleted, or the page holds more than {CASES}"
    );
}

#[test]
fn every_whole_answer_gets_the_first_refusal_the_contract_says() {
    let document = document();
    let cases = document["wholeAnswers"].as_array().unwrap();
    assert_eq!(cases.len(), 14);
    for case in cases {
        let free_input = case["freeInput"].as_bool().unwrap();
        let form = form_of(case["questions"].as_array().unwrap(), free_input).unwrap();
        let spec = spec_of(&form, free_input);
        let got = check_answer(&spec, &case["context"]).err().map(|verdict| {
            (
                verdict.question.unwrap_or_else(|| "*".to_owned()),
                verdict.failure.rule.as_str().to_owned(),
            )
        });
        let want = case["refusal"].as_array().map(|pair| {
            (
                pair[0].as_str().unwrap().to_owned(),
                pair[1].as_str().unwrap().to_owned(),
            )
        });
        assert_eq!(got, want, "{}", case["context"]);
    }
}

#[test]
fn an_invalid_question_is_refused_before_a_form_is_drawn() {
    let document = document();
    let cases = document["invalidQuestions"].as_array().unwrap();
    assert_eq!(cases.len(), 34);
    for case in cases {
        let because = case["because"].as_str().unwrap();
        let outcome = form_of(&[case["question"].clone()], false);
        if because == "a bound that is a JSON number" {
            // The model's arguments may write a number as a JSON number: the form says it as the
            // string the catalog needs, so nothing invalid is ever drawn.
            let form = outcome.unwrap();
            assert_eq!(form.questions[0].ext.max_value.as_deref(), Some("5"));
            continue;
        }
        assert!(
            outcome.is_err(),
            "{because}: {} was accepted",
            case["question"]
        );
    }
}

#[test]
fn the_contracts_examples_and_the_surfaces_it_draws_are_questions_the_agent_makes() {
    let document = document();
    for question in document["questions"].as_array().unwrap() {
        let form = form_of(&[question.clone()], false).unwrap();
        // A question that is a valid answer-able form judges something.
        assert!(spec_of(&form, false).judges_anything(), "{question}");
    }
    for surface in document["surfaces"].as_array().unwrap() {
        let drawn = surface["result"] == "drawn";
        assert_eq!(
            form_of(&[surface["question"].clone()], false).is_ok(),
            drawn,
            "{}",
            surface["question"]
        );
    }
}

#[test]
fn the_contracts_example_answer_is_an_answer_to_its_example_form() {
    let document = document();
    let questions = document["questions"].as_array().unwrap();
    let form = form_of(questions, false).unwrap();
    let spec = spec_of(&form, false);
    assert_eq!(check_answer(&spec, &document["answers"]["context"]), Ok(()));
}
