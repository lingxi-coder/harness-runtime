use super::{
    named_prompt_sections_for_mode, render_prompt_sections, FileTree, SystemPromptContext,
};
use lingxi_core::host::session_context::ConversationMode;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const UPSTREAM_AVAILABILITY: &str = " - LingXi is available as a CLI in the terminal, desktop app (Mac/Windows), web app (claude.ai/code), and IDE extensions (VS Code, JetBrains).";
const HOST_AVAILABILITY: &str = " - LingXi runs in the current host client. Use the interfaces and capabilities explicitly exposed in this session.";

fn assert_native_body(case_name: &str) {
    let oracle: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/prompt_session_2_1_319.json"
    ))
    .unwrap();
    assert_eq!(oracle["version"], "2.1.319");
    assert_eq!(oracle["host_line_allowlist"].as_array().unwrap().len(), 1);
    assert_eq!(
        oracle["host_line_allowlist"][0]["upstream"],
        UPSTREAM_AVAILABILITY
    );
    assert_eq!(oracle["host_line_allowlist"][0]["host"], HOST_AVAILABILITY);
    let case = oracle["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"].as_str() == Some(case_name))
        .expect("native conversation case exists");
    let input = &case["input"];
    let model = input["model"].as_str().unwrap();
    let ctx = SystemPromptContext {
        native_system_turns: None,
        prompt_policy: None,
        cwd: PathBuf::from("/repo"),
        platform: "darwin".into(),
        model: model.to_owned(),
        model_marketing_name: super::env_meta::marketing_name_for_model(model).map(str::to_owned),
        knowledge_cutoff: super::env_meta::knowledge_cutoff_for_model(model).map(str::to_owned),
        shell: "zsh".into(),
        os_version: "Darwin 27.2.0".into(),
        git_status: None,
        in_worktree: false,
        file_tree: FileTree::default(),
        memory_files: Vec::new(),
        tool_names: input["tool_names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect(),
        skills_available: input["skills_available"].as_bool().unwrap(),
        is_interactive: input["is_interactive"].as_bool().unwrap(),
        memory_dir: input["memory_dir"].as_str().map(PathBuf::from),
        exclude_dynamic_sections: input["exclude_dynamic_sections"].as_bool().unwrap(),
    };
    let mut sections = named_prompt_sections_for_mode(&ctx, None, ConversationMode::Code);
    assert_eq!(sections.first().unwrap().id, "header");
    sections.remove(0);
    let actual = render_prompt_sections(&sections);
    let upstream = case["expected_body"].as_str().unwrap();
    let expected_hash = format!("{:x}", Sha256::digest(upstream.as_bytes()));
    assert_eq!(
        expected_hash,
        case["expected_body_sha256"].as_str().unwrap()
    );
    assert_eq!(
        upstream
            .split('\n')
            .filter(|line| *line == UPSTREAM_AVAILABILITY)
            .count(),
        1
    );
    let expected = upstream
        .split('\n')
        .map(|line| {
            if line == UPSTREAM_AVAILABILITY {
                HOST_AVAILABILITY
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        format!("{:x}", Sha256::digest(expected.as_bytes())),
        case["expected_host_body_sha256"].as_str().unwrap()
    );
    if actual != expected {
        let mismatch = actual
            .as_bytes()
            .iter()
            .zip(expected.as_bytes())
            .position(|(actual, expected)| actual != expected)
            .unwrap_or_else(|| actual.len().min(expected.len()));
        let excerpt = |value: &str| {
            String::from_utf8_lossy(
                &value.as_bytes()[mismatch.saturating_sub(80).min(value.len())
                    ..(mismatch + 220).min(value.len())],
            )
            .into_owned()
        };
        panic!(
            "{case_name}: complete native body differs at byte {mismatch}; actual {} bytes, expected {} bytes; sections {:?}\nactual: {:?}\nexpected: {:?}",
            actual.len(), expected.len(),
            sections.iter().map(|section| section.id.as_str()).collect::<Vec<_>>(),
            excerpt(&actual), excerpt(&expected),
        );
    }
}

#[test]
fn native_baseline_319_sonnet_46_memory_off() {
    assert_native_body("claude-sonnet-4-6-memory-off");
}

#[test]
fn native_baseline_319_sonnet_46_memory_on() {
    assert_native_body("claude-sonnet-4-6-memory-on");
}

#[test]
fn native_baseline_319_opus_5_memory_off() {
    assert_native_body("claude-opus-5-memory-off");
}

#[test]
fn native_baseline_319_opus_5_memory_on() {
    assert_native_body("claude-opus-5-memory-on");
}

#[test]
fn native_baseline_319_sonnet_55_memory_off() {
    assert_native_body("claude-sonnet-5-5-memory-off");
}

#[test]
fn native_baseline_319_sonnet_55_memory_on() {
    assert_native_body("claude-sonnet-5-5-memory-on");
}
