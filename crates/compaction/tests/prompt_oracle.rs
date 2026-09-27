use compaction::{format_compact_summary, get_compact_prompt, get_compact_user_summary_message};
use serde_json::Value;

#[test]
fn oracle_261_prompt_and_continuation_bytes() {
    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/claude_2_1_261_prompt.json")).unwrap();
    assert_eq!(compaction::BASE_COMPACT_PROMPT, oracle["basePrompt"]);
    for case in oracle["prompts"].as_array().unwrap() {
        assert_eq!(
            get_compact_prompt(case["custom"].as_str()),
            case["expected"].as_str().unwrap(),
            "custom instructions: {:?}",
            case["custom"]
        );
    }
    for case in oracle["continuations"].as_array().unwrap() {
        assert_eq!(
            get_compact_user_summary_message(
                case["raw"].as_str().unwrap(),
                case["suppressFollowUpQuestions"].as_bool().unwrap(),
                case["transcriptPath"].as_str(),
                case["recentMessagesPreserved"].as_bool().unwrap(),
            ),
            case["expected"].as_str().unwrap(),
            "continuation: {case}"
        );
    }
}

#[test]
fn oracle_261_summary_formatting_bytes() {
    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/claude_2_1_261_prompt.json")).unwrap();
    for case in oracle["summaries"].as_array().unwrap() {
        assert_eq!(
            format_compact_summary(case["raw"].as_str().unwrap()),
            case["expected"].as_str().unwrap(),
            "raw summary: {:?}",
            case["raw"]
        );
    }
}
