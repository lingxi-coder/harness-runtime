//! Regression fixtures for interrupted parallel-tool transcript recovery.

use serde_json::Value;
use session::jsonl::loader::find_tip;
use session::jsonl::{build_conversation_chain, route_lines};

#[test]
fn resume_matches_executed_claude_2_1_286_recovery_fixtures() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/recovery-2.1.286/cases.json")).unwrap();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let raw = case["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| serde_json::to_string(row).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let loaded = route_lines(&raw);
        let tip = find_tip(&loaded, "fixture-session").map(|row| row.uuid.as_str());
        assert_eq!(tip, case["tip"].as_str(), "{name}: selected leaf");
        let (chain, _) = build_conversation_chain(&loaded, "fixture-session");
        let expected: Vec<_> = case["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap())
            .collect();
        assert_eq!(
            chain
                .iter()
                .map(|row| row.uuid.as_str())
                .collect::<Vec<_>>(),
            expected,
            "{name}: recovered order"
        );
        for row in chain {
            assert_eq!(
                row.message, loaded.by_uuid[&row.uuid].message,
                "{name}: recovery must preserve message payload"
            );
        }
    }
}
