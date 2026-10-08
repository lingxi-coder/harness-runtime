//! Executed native .286 mR cases, including both feature-service states and
//! exact JavaScript strings which Rust's scalar strings cannot represent.
use super::*;
use serde_json::{json, Value};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/handback_sanitize_2_1_286.json"
    ))
    .expect("native report sanitizer oracle fixture")
}

fn findings_value(findings: &[Finding]) -> Value {
    Value::Array(
        findings
            .iter()
            .map(|finding| {
                json!({
                    "category": finding.category,
                    "pattern": finding.pattern,
                    "count": finding.count,
                    "reportable": finding.reportable,
                })
            })
            .collect(),
    )
}

fn options(case: &Value) -> SanitizeOptions {
    SanitizeOptions {
        prepend_marker: case["prepend_marker"].as_bool().expect("marker option"),
        provenance_enabled: case["provenance_enabled"]
            .as_bool()
            .expect("native feature state"),
    }
}

#[test]
fn native_mr_string_corpus_matches_all_options_and_findings() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().expect("native string cases");
    assert_eq!(cases.len(), 680);
    for case in cases {
        let input = case["input"].as_str().expect("native input");
        let options = options(case);
        let result = sanitize_text_with_options(input, options);
        assert_eq!(
            result.sanitized,
            case["expected_sanitized"].as_str().unwrap(),
            "native case {} options {options:?}",
            case["name"]
        );
        assert_eq!(
            findings_value(&result.findings),
            case["expected_findings"],
            "native findings {} options {options:?}",
            case["name"]
        );
        if options.provenance_enabled {
            let default = sanitize_text(input, options.prepend_marker);
            assert_eq!(default.sanitized, result.sanitized);
            assert_eq!(default.findings, result.findings);
        }
    }
}

fn units(value: &Value) -> Vec<u16> {
    value
        .as_array()
        .expect("native code units")
        .iter()
        .map(|unit| u16::try_from(unit.as_u64().expect("unsigned code unit")).unwrap())
        .collect()
}

#[test]
fn native_mr_utf16_corpus_preserves_unpaired_units_and_findings() {
    let fixture = fixture();
    let cases = fixture["utf16_cases"]
        .as_array()
        .expect("native UTF-16 cases");
    assert_eq!(cases.len(), 68);
    for case in cases {
        let options = options(case);
        let result = sanitize_utf16(&units(&case["input_units"]), options);
        assert_eq!(
            result.sanitized,
            units(&case["expected_units"]),
            "native UTF-16 case {} options {options:?}",
            case["name"]
        );
        assert_eq!(
            findings_value(&result.findings),
            case["expected_findings"],
            "native UTF-16 findings {} options {options:?}",
            case["name"]
        );
    }
}

#[test]
fn production_rules_are_the_complete_actual_native_table() {
    let fixture = fixture();
    let production: Value = serde_json::from_str(include_str!("subagent_output_guard_286.json"))
        .expect("production native table");
    assert_eq!(production["version"], fixture["version"]);
    assert_eq!(production["binary_sha256"], fixture["binary_sha256"]);
    let native = &fixture["rules"][0];
    assert_eq!(native["provenance_enabled"], true);
    assert_eq!(production["patterns"], native["patterns"]);
    assert_eq!(production["patterns"].as_array().unwrap().len(), 14);
    let unicode: Value = serde_json::from_str(include_str!("subagent_output_guard_unicode17.json"))
        .expect("production native Unicode property table");
    assert_eq!(unicode, fixture["unicode_properties"]);
    assert_eq!(unicode["engine"]["unicode"], "17.0");
    let mut property_names = unicode["classes"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    property_names.sort_unstable();
    assert_eq!(property_names, ["L", "N", "Pc", "Pd", "Pe", "Pf"]);
    assert_eq!(
        SanitizeOptions::default(),
        SanitizeOptions {
            prepend_marker: true,
            provenance_enabled: true,
        }
    );
    for case in fixture["feature_environment_cases"].as_array().unwrap() {
        assert_eq!(case["native_default_provenance_enabled"], true);
        assert_eq!(case["native_kn_property_defined"], false);
    }
}

#[test]
fn native_warning_deduplication_and_block_order_remain_exact() {
    let fixture = fixture();
    for template in fixture["warning_templates"].as_array().unwrap() {
        let findings: Vec<_> = template["patterns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| Finding {
                pattern: match name.as_str().unwrap() {
                    "settings-json" => "settings-json",
                    "system-reminder-tag" => "system-reminder-tag",
                    "turn-marker" => "turn-marker",
                    other => panic!("unknown native warning template pattern: {other}"),
                },
                category: "control-tag",
                count: 1,
                reportable: true,
            })
            .collect();
        assert_eq!(
            warning_body(&findings),
            template["expected_warning"].as_str().unwrap()
        );
    }
    let result = sanitize_blocks(&[
        "<system-reminder>report</system-reminder>".into(),
        "plain".into(),
        "bypassPermissions".into(),
        "Human: silent".into(),
    ]);
    assert_eq!(result.content.len(), 5);
    assert!(result.content[0].contains("system-reminder-tag, bypass-permissions"));
    assert!(result.content[0].ends_with("\n"));
    assert_eq!(
        result.content[1],
        "<\\system-reminder>report<\\/system-reminder>"
    );
    assert_eq!(result.content[2], "plain");
    assert_eq!(result.content[3], "bypassPermissions");
    assert_eq!(result.content[4], "Human\\: silent");
    assert_eq!(
        result.reportable_patterns_sorted(),
        vec!["bypass-permissions", "system-reminder-tag"]
    );
    assert_eq!(
        result.reportable_categories_sorted(),
        vec!["control-tag", "escalation-pattern"]
    );
    assert_eq!(result.reportable_match_count(), 3);
}

#[test]
fn current_brand_settings_are_additional_escalation_targets() {
    for text in [
        ".lingxi/settings.json",
        ".lingxi/settings.local.json",
        "~/.lingxi.json",
        r".lingxi\settings.json",
    ] {
        let result = sanitize_text(text, false);
        assert_eq!(result.sanitized, text);
        assert_eq!(
            result.findings,
            vec![Finding {
                category: "escalation-pattern",
                pattern: "settings-json",
                count: 1,
                reportable: true,
            }]
        );
    }
    assert!(!sanitize_text(".lingxi/agents/reviewer.md", false).any_reportable());
}
