//! Typed decision extension boundaries, without a production model backend.

use std::collections::BTreeMap;

use harness_runtime::models::decision::{Answer, ModelRef, Question, Request, Response, Usage};

fn request() -> Request {
    Request {
        model: ModelRef {
            backend: "fixture".into(),
            account: "account-a".into(),
            model: "decision-fixture".into(),
        },
        state_revision: 7,
        state: serde_json::json!({"task": "choose a route"}),
        questions: BTreeMap::from([
            (
                "safe".into(),
                Question::Binary {
                    instructions: "Is this safe?".into(),
                },
            ),
            (
                "route".into(),
                Question::Choice {
                    instructions: "Choose a route".into(),
                    options: BTreeMap::from([
                        ("a".into(), "Route A".into()),
                        ("b".into(), "Route B".into()),
                    ]),
                },
            ),
            (
                "priority".into(),
                Question::Score {
                    instructions: "Rate urgency".into(),
                    levels: vec!["low".into(), "high".into()],
                },
            ),
        ]),
    }
}

fn response() -> Response {
    Response {
        state_revision: 7,
        answers: BTreeMap::from([
            ("safe".into(), Answer::Binary { probability: 0.8 }),
            (
                "route".into(),
                Answer::Choice {
                    option: "a".into(),
                    probabilities: None,
                    confidence: None,
                },
            ),
            (
                "priority".into(),
                Answer::Score {
                    value: 0.5,
                    probabilities: Some(vec![0.5, 0.5]),
                    confidence: None,
                },
            ),
        ]),
        usage: Usage {
            input_tokens: Some(20),
            output_tokens: None,
        },
    }
}

#[test]
fn mixed_decisions_preserve_missing_statistics_and_usage() {
    let response = response();
    response.validate_for(&request()).unwrap();
    let roundtrip: Response =
        serde_json::from_str(&serde_json::to_string(&response).unwrap()).unwrap();
    assert_eq!(roundtrip, response);
    assert_eq!(roundtrip.usage.output_tokens, None);
    assert!(matches!(
        roundtrip.answers["route"],
        Answer::Choice {
            confidence: None,
            probabilities: None,
            ..
        }
    ));
}

#[test]
fn stale_revision_and_missing_or_extra_questions_are_rejected() {
    let mut stale = response();
    stale.state_revision = 6;
    assert!(stale.validate_for(&request()).is_err());
    let mut missing = response();
    missing.answers.remove("safe");
    assert!(missing.validate_for(&request()).is_err());
    let mut extra = response();
    extra
        .answers
        .insert("other".into(), Answer::Binary { probability: 0.5 });
    assert!(extra.validate_for(&request()).is_err());
}

#[test]
fn mismatched_answer_type_and_unknown_choice_are_rejected() {
    let mut result = response();
    result.answers.insert(
        "safe".into(),
        Answer::Score {
            value: 0.5,
            probabilities: None,
            confidence: None,
        },
    );
    assert!(result.validate_for(&request()).is_err());
    let mut result = response();
    result.answers.insert(
        "route".into(),
        Answer::Choice {
            option: "outside".into(),
            probabilities: None,
            confidence: None,
        },
    );
    assert!(result.validate_for(&request()).is_err());
}

#[test]
fn invalid_probability_values_and_incomplete_distributions_are_rejected() {
    for value in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
        let mut result = response();
        result
            .answers
            .insert("safe".into(), Answer::Binary { probability: value });
        assert!(result.validate_for(&request()).is_err());
    }
    for probabilities in [
        BTreeMap::from([("a".into(), 1.0)]),
        BTreeMap::from([("a".into(), 0.8), ("b".into(), 0.8)]),
    ] {
        let mut result = response();
        result.answers.insert(
            "route".into(),
            Answer::Choice {
                option: "a".into(),
                probabilities: Some(probabilities),
                confidence: None,
            },
        );
        assert!(result.validate_for(&request()).is_err());
    }
}

#[test]
fn score_domain_and_confidence_are_validated() {
    for (value, confidence) in [(2.0, None), (f64::NAN, None), (0.5, Some(f64::NAN))] {
        let mut result = response();
        result.answers.insert(
            "priority".into(),
            Answer::Score {
                value,
                probabilities: None,
                confidence,
            },
        );
        assert!(result.validate_for(&request()).is_err());
    }
}

#[test]
fn empty_bindings_questions_and_rubrics_are_rejected_before_dispatch() {
    let mut input = request();
    input.model.account.clear();
    assert!(input.validate().is_err());
    let mut input = request();
    input.questions.clear();
    assert!(input.validate().is_err());
    let mut input = request();
    input.questions.insert(
        "priority".into(),
        Question::Score {
            instructions: "score".into(),
            levels: vec![],
        },
    );
    assert!(response().validate_for(&input).is_err());
}
