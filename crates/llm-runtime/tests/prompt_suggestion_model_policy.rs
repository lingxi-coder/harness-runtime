//! Native prompt-suggestion classifier model-policy oracle tests.

use llm_runtime::model::classifier_model_policy::*;
use std::collections::BTreeMap;

#[test]
fn native_classifier_model_selection_and_denial_oracle() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/prompt_suggestion_model_2_1_296.json"
    ))
    .unwrap();
    let mut mismatches = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let input: ClassifierModelSelection =
            serde_json::from_value(case["input"].clone()).unwrap();
        let (mut actual, restriction) = select_classifier_model(&input);
        if restriction != ClassifierModelRestriction::None
            && (case["input"]["denied"] == true
                || (restriction == ClassifierModelRestriction::DeniedOrExactAllowlist
                    && case["input"]["exact_disallowed"] == true))
        {
            actual = input.main_model.clone();
        }
        if actual != case["expected"].as_str().unwrap() {
            mismatches.push(format!("selection {case}: {actual}"));
        }
    }
    for case in fixture["deniedCases"].as_array().unwrap() {
        let actual = classifier_model_denied(
            case["model"].as_str().unwrap(),
            &[case["rule"].as_str().unwrap().into()],
            &BTreeMap::new(),
        );
        if actual != case["expected"].as_bool().unwrap() {
            mismatches.push(format!("deny {case}: {actual}"));
        }
    }
    for case in fixture["exactCases"].as_array().unwrap() {
        let actual = classifier_model_exact_allowed(
            case["model"].as_str().unwrap(),
            &[case["rule"].as_str().unwrap().into()],
            &BTreeMap::new(),
        );
        if actual != case["expected"].as_bool().unwrap() {
            mismatches.push(format!("exact {case}: {actual}"));
        }
    }
    for case in fixture["canonicalCases"].as_array().unwrap() {
        let actual = is_canonical_haiku(case["model"].as_str().unwrap());
        if actual != case["expected"].as_bool().unwrap() {
            mismatches.push(format!("identity {case}: {actual}"));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn provider_identity_and_override_maps_are_authoritative() {
    let mut input = ClassifierModelSelection {
        main_model: "claude-named-openai-model".into(),
        small_fast_override: Some("claude-haiku-4-5".into()),
        ..Default::default()
    };
    assert_eq!(
        select_classifier_model(&input).0,
        input.main_model,
        "a Claude-like model name is not provider metadata"
    );
    input.route.provider = Some("firstParty".into());
    assert_eq!(select_classifier_model(&input).0, "claude-haiku-4-5");
    let overrides = BTreeMap::from([("claude-haiku-4-5".into(), "deployment-custom".into())]);
    assert!(classifier_model_denied(
        "deployment-custom",
        &["haiku".into()],
        &overrides
    ));
    assert!(!classifier_model_denied(
        "deployment-custom",
        &["opus".into()],
        &overrides
    ));
}
