use std::sync::Arc;

use tool_api::tool_trait::{PromptOptions, Tool};
use tool_file::{FileEditTool, FileReadTool, FileWriteTool, GlobTool, GrepTool};

struct PromptEnv(Vec<(&'static str, Option<String>)>);

impl Drop for PromptEnv {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[tokio::test]
async fn dispatched_claude_file_prompts_match_the_319_artifact() {
    let keys = [
        "CLAUDE_CODE_MODEL_CAPABILITIES",
        "CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT",
        "LINGXI_SIMPLE_SYSTEM_PROMPT",
    ];
    let _env = PromptEnv(
        keys.iter()
            .map(|key| (*key, std::env::var(key).ok()))
            .collect(),
    );
    for key in keys {
        std::env::remove_var(key);
    }
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../scripts/tests/fixtures/tool_prompts_319.json"
    ))
    .unwrap();
    let ctx = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        Vec::new(),
    );
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(FileReadTool::new(ctx.clone())),
        Box::new(FileWriteTool::new(ctx.clone())),
        Box::new(FileEditTool::new(ctx.clone())),
        Box::new(GlobTool::new(ctx.clone())),
        Box::new(GrepTool::new(ctx)),
    ];
    for tool in &tools {
        for (lean, model) in [(false, "claude-sonnet-4-6"), (true, "claude-opus-5-5")] {
            let expected = fixture["cases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["tool"] == tool.name() && row["lean"] == lean)
                .unwrap()["output"]
                .as_str()
                .unwrap()
                .replace(
                    "This tool allows Claude Code to read images (eg PNG, JPG, etc). When reading an image file the contents are presented visually as Claude Code is a multimodal LLM.",
                    "This tool reads images (eg PNG, JPG, etc). When reading an image file, the contents are presented visually to the selected model.",
                )
                .replace("as a Bash command", "through a registered shell tool")
                .replace("via Bash", "via a registered shell tool");
            let prompt = tool
                .prompt(&PromptOptions {
                    prompt_policy: None,
                    model: Some(model.into()),
                    ..Default::default()
                })
                .await;
            assert_eq!(
                prompt.string_at(""),
                Some(expected.as_str()),
                "{} lean={lean}",
                tool.name(),
            );
        }
    }
    std::env::set_var("CLAUDE_CODE_MODEL_CAPABILITIES", "temporal_hare");
    for row in fixture["rewriteCases"].as_array().unwrap() {
        let tool = tools
            .iter()
            .find(|tool| row["tool"] == tool.name())
            .unwrap();
        let prompt = tool
            .prompt(&PromptOptions {
                prompt_policy: None,
                model: Some("claude-opus-5-5".into()),
                ..Default::default()
            })
            .await;
        let expected = row["output"]
            .as_str()
            .unwrap()
            .replace("via Bash", "via a registered shell tool")
            .replace("file links in Claude Code.", "file links in LingXi.");
        assert_eq!(
            prompt.string_at(""),
            Some(expected.as_str()),
            "{} rewrite",
            tool.name()
        );
    }

    // A forced Claude rewrite capability must not affect another bundle. The
    // stale legacy model intentionally disagrees with each resolved route.
    for mode in [
        lingxi_core::host::session_context::ConversationMode::Code,
        lingxi_core::host::session_context::ConversationMode::Chat,
    ] {
        for (provider, model) in [
            ("openai", "gpt-5.5"),
            ("moonshot", "kimi-k2.5"),
            ("google", "gemini-3.5-flash"),
            ("custom", "vendor/unknown-model"),
        ] {
            let target = lingxi_core::host::prompt_policy::PromptTarget::from_route(
                mode, provider, model, None, None, None,
            );
            let policy = Arc::new(lingxi_core::host::prompt_policy::resolve_prompt_policy(
                target,
            ));
            let options = PromptOptions {
                prompt_policy: Some(policy),
                model: Some("claude-opus-5-5".into()),
                ..Default::default()
            };
            for tool in &tools {
                let expected = fixture["cases"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["tool"] == tool.name() && row["lean"] == true)
                    .unwrap()["output"]
                    .as_str()
                    .unwrap()
                    .replace("as a Bash command", "through a registered shell tool")
                    .replace("via Bash", "via a registered shell tool");
                assert_eq!(
                    tool.prompt(&options).await.string_at(""),
                    Some(expected.as_str()),
                    "{} mode={mode:?} provider={provider} model={model}",
                    tool.name(),
                );
            }
        }
    }
}
