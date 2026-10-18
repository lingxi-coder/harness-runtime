//! Differential controls and lifecycle fixtures from shipped Claude Code 2.1.296.
#[allow(dead_code)]
#[path = "../src/conversation/prompt_suggestion_state.rs"]
mod state;

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;
use state::*;

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "fixtures/prompt_suggestion_controls_2_1_296.json"
    ))
    .unwrap()
}

#[derive(Deserialize)]
struct RawPresentationFixture {
    #[serde(rename = "presentationCases")]
    presentation_cases: Vec<RawPresentationCase>,
    #[serde(rename = "transitionCases")]
    transition_cases: Vec<RawTransitionCase>,
}

#[derive(Deserialize)]
struct RawPresentationCase {
    state: Box<RawValue>,
    input: String,
    now: f64,
    expected: Box<RawValue>,
    after: Box<RawValue>,
}

#[derive(Deserialize)]
struct RawTransitionCase {
    actions: Vec<String>,
    expected: Box<RawValue>,
}

fn raw_presentation_fixture() -> RawPresentationFixture {
    serde_json::from_str(include_str!(
        "fixtures/prompt_suggestion_controls_2_1_296.json"
    ))
    .unwrap()
}

fn boolean(value: &Value, key: &str) -> bool {
    value[key].as_bool().unwrap()
}
fn number(value: &Value, key: &str) -> f64 {
    value[key].as_f64().unwrap()
}
fn maybe_number(value: &Value) -> f64 {
    value.as_f64().unwrap_or(f64::NAN)
}

#[test]
fn native_environment_and_switch_precedence() {
    let fixtures = fixture();
    for case in fixtures["envCases"].as_array().unwrap() {
        assert_eq!(
            parse_env(case["raw"].as_str()),
            case["expected"].as_bool(),
            "{case}"
        );
    }
    for case in fixtures["initialCases"].as_array().unwrap() {
        let actual = initial_enablement(
            case["env"].as_bool(),
            boolean(case, "rollout"),
            boolean(case, "non_interactive"),
            boolean(case, "swarm_teammate"),
            case["setting"].as_bool(),
        );
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            case["expected"],
            "{case}"
        );
    }
    for case in fixtures["explicitCases"].as_array().unwrap() {
        assert_eq!(
            explicit_enablement(case["env"].as_bool(), case["setting"].as_bool()),
            boolean(case, "expected"),
            "{case}"
        );
    }
}

#[test]
fn native_generation_gates_and_cache_boundaries() {
    for case in fixture()["gateCases"].as_array().unwrap() {
        let gates = GenerationGates {
            aborted: boolean(case, "aborted"),
            assistant_count: case["assistant_count"].as_u64().unwrap() as usize,
            last_response_api_error: boolean(case, "last_response_api_error"),
            enabled: boolean(case, "enabled"),
            pending_permission: boolean(case, "pending_permission"),
            elicitation_active: boolean(case, "elicitation_active"),
            plan_mode: boolean(case, "plan_mode"),
            rate_limit_status: case["rate_limit_status"].as_str().unwrap(),
            env: case["env"].as_bool(),
            usage: serde_json::from_value(case["usage"].clone()).unwrap(),
            cache_thaw: boolean(case, "cache_thaw"),
        };
        let actual = gates.suppression();
        assert_eq!(
            actual.map(Suppression::reason),
            case["expected"]["reason"].as_str(),
            "{case}"
        );
        assert_eq!(
            actual.and_then(Suppression::cache_cold_by),
            case["expected"]["cacheColdBy"].as_str(),
            "{case}"
        );
    }
}

#[test]
fn native_backoff_age_threshold_env_and_notice_boundaries() {
    for case in fixture()["backoffCases"].as_array().unwrap() {
        let now_ms = 2_000_000_000_000.0;
        let first_start_ms = if case["age"].is_null() {
            None
        } else {
            Some(case["age"].as_f64().map_or(f64::NAN, |age| now_ms - age))
        };
        let streak = maybe_number(&case["streak"]);
        let actual = BackoffInputs {
            persisted_streak: streak,
            now_ms,
            first_start_ms,
            env: case["env"].as_bool(),
        }
        .backing_off();
        assert_eq!(actual, boolean(case, "expected"), "{case}");
        let notice = claim_notice(streak, actual);
        assert_eq!(notice.is_some(), boolean(case, "notice_claimed"), "{case}");
        if let Some(update) = notice {
            assert_eq!(update, number(case, "notice_streak"), "{case}");
        }
    }
    assert!(!BackoffInputs {
        persisted_streak: f64::INFINITY,
        now_ms: 2e12,
        first_start_ms: None,
        env: None
    }
    .backing_off());
    let mut state = UnusedState::default();
    for index in 0..23 {
        assert_eq!(state.eligible(true), index % 10 == 0);
        assert_eq!(state.eligible_turns, index + 1);
    }
    assert!(state.eligible(false));
    assert_eq!(state.eligible_turns, 23);
    state.ask_on_next_turn();
    assert!(state.eligible(true));
    assert_eq!(state.eligible_turns, 1);
    for case in fixture()["eligibleCases"].as_array().unwrap() {
        let mut state = UnusedState {
            eligible_turns: case["eligible_turns"].as_u64().unwrap(),
            unused_in_a_row: 17,
        };
        assert_eq!(
            state.eligible(boolean(case, "backing_off")),
            boolean(case, "expected"),
            "{case}"
        );
        assert_eq!(
            state.eligible_turns,
            case["after_eligible"].as_u64().unwrap(),
            "{case}"
        );
        state.ask_on_next_turn();
        assert_eq!(
            state.eligible_turns,
            case["after_ask"].as_u64().unwrap(),
            "{case}"
        );
        assert_eq!(
            state.unused_in_a_row,
            case["unused_after_ask"].as_u64().unwrap(),
            "{case}"
        );
    }
}

#[test]
fn native_submission_and_config_toggle_streak_updates() {
    let fixtures = fixture();
    for case in fixtures["submissionCases"].as_array().unwrap() {
        let mut state = UnusedState {
            eligible_turns: 7,
            unused_in_a_row: case["unused"].as_u64().unwrap(),
        };
        let actual =
            state.record_submission(boolean(case, "accepted"), maybe_number(&case["streak"]));
        assert_eq!(actual, case["update"].as_f64(), "{case}");
        assert_eq!(
            state.unused_in_a_row,
            case["expected_unused"].as_u64().unwrap(),
            "{case}"
        );
        assert_eq!(state.eligible_turns, 7);
    }
    for case in fixtures["reenableCases"].as_array().unwrap() {
        let mut state = UnusedState {
            eligible_turns: 7,
            unused_in_a_row: 19,
        };
        assert_eq!(
            state.reenable(maybe_number(&case["streak"])),
            case["update"].as_f64(),
            "{case}"
        );
        assert_eq!(
            state.unused_in_a_row,
            case["expected_unused"].as_u64().unwrap(),
            "{case}"
        );
        assert_eq!(state.eligible_turns, 7);
    }
}

#[test]
fn native_presentation_transitions_and_edited_acceptance() {
    let fixtures = raw_presentation_fixture();
    for case in fixtures.presentation_cases {
        let mut state: Presentation = serde_json::from_str(case.state.get()).unwrap();
        let expected: Value = serde_json::from_str(case.expected.get()).unwrap();
        let actual = state.submission(&case.input, case.now, false);
        if let Some(actual) = actual {
            assert_eq!(
                actual.accepted,
                boolean(&expected, "accepted"),
                "{}",
                case.state.get()
            );
            assert_eq!(
                actual.accept_method,
                expected["acceptMethod"].as_str(),
                "{}",
                case.state.get()
            );
            assert_eq!(
                actual.elapsed_ms,
                number(&expected, "elapsedMs"),
                "{}",
                case.state.get()
            );
            assert_eq!(
                actual.similarity,
                number(&expected, "similarity"),
                "{}",
                case.state.get()
            );
            assert_eq!(
                actual.generation_request_id.as_deref(),
                expected["generationRequestId"].as_str(),
                "{}",
                case.state.get()
            );
        } else {
            assert!(expected.is_null(), "{}", case.state.get());
        }
        let after: Presentation = serde_json::from_str(case.after.get()).unwrap();
        assert_eq!(state, after, "{}", case.state.get());
    }
    for case in fixtures.transition_cases {
        let mut state = Presentation::Generated {
            text: "run tests".into(),
            generation_request_id: None,
        };
        for (index, action) in case.actions.iter().enumerate() {
            if action == "shown" {
                state.mark_shown(100.0 + index as f64);
            } else {
                state.mark_accepted(100.0 + index as f64);
            }
        }
        let expected: Presentation = serde_json::from_str(case.expected.get()).unwrap();
        assert_eq!(state, expected, "{}", case.expected.get());
    }
}

#[test]
fn native_focus_and_background_blocked_throttle_bypass() {
    for case in fixture()["focusCases"].as_array().unwrap() {
        let main_thread = case["query_source"]
            .as_str()
            .is_some_and(|v| v.starts_with("repl_main_thread"));
        let remote = boolean(case, "background_mode");
        let blocked = boolean(case, "background_blocked_without_block");
        let actual = main_thread
            .then(|| {
                cli_focus_suppression(case["current_focus"].as_str().unwrap(), remote, blocked)
            })
            .flatten();
        assert_eq!(actual, case["expected"].as_str(), "{case}");
        assert_eq!(
            main_thread && actual.is_none(),
            boolean(case, "query_called"),
            "{case}"
        );
        assert_eq!(
            main_thread && actual.is_none() && !(remote && blocked),
            boolean(case, "passed_throttle"),
            "{case}"
        );
    }
}
