//! Byte-lock for [`env_block::format`] — the `# Environment` markdown block
//! (claude-code v2.1.183 `Kym` / `env_info_simple`, the MAIN/J0 path).

use orchestrator::prompt::{env_block, FileTree, SystemPromptContext};
use std::path::PathBuf;

fn ctx_minimal() -> SystemPromptContext {
    SystemPromptContext {
        cwd: PathBuf::from("/Users/u/proj"),
        platform: "darwin".into(),
        model: "claude-opus-4-7".into(),
        model_marketing_name: Some("Opus 4.7".into()),
        knowledge_cutoff: Some("January 2026".into()),
        shell: "zsh".into(),
        os_version: "Darwin 25.3.0".into(),
        git_status: None,
        in_worktree: false,
        file_tree: FileTree::default(),
        memory_files: Vec::new(),
        tool_names: Vec::new(),
        skills_available: false,
        is_interactive: false,
        memory_dir: None,
        exclude_dynamic_sections: false,
    }
}

#[test]
fn env_block_full_byte_lock() {
    let out = env_block::format(&ctx_minimal());
    // The `# Environment` block, byte-exact against `Kym`. Built via explicit
    // `\n`-joined lines so the TRAILING SPACE on the "You have been invoked …: "
    // line survives source formatting. Each subsequent line carries the ` - `
    // bullet (claude-code `AG`). The em-dash in "Model IDs —" is U+2014.
    let expected = [
        "# Environment",
        "You have been invoked in the following environment: ", // trailing space
        " - Primary working directory: /Users/u/proj",
        " - Is a git repository: false",
        " - Platform: darwin",
        " - Shell: zsh",
        " - OS Version: Darwin 25.3.0",
        " - You are powered by the model named Opus 4.7. The exact model ID is claude-opus-4-7.",
        " - Assistant knowledge cutoff is January 2026.",
        " - The most recent Claude models are the Claude 5 family and Haiku 4.5. \
Model IDs \u{2014} Fable 5.1: 'claude-fable-5-1', Opus 5: 'claude-opus-5', \
Sonnet 5: 'claude-sonnet-5', Haiku 4.5: 'claude-haiku-4-5-20251001'. \
When building AI applications, default to the latest and most capable Claude models.",
        " - LingXi is available as a CLI in the terminal, desktop app (Mac/Windows), \
web app (claude.ai/code), and IDE extensions (VS Code, JetBrains).",
        " - Fast mode for LingXi uses Claude Opus with faster output \
(it does not downgrade to a smaller model). It can be toggled with /fast and is \
available on Opus 5/4.8.",
    ]
    .join("\n");
    assert_eq!(out, expected, "env_block byte-lock mismatch");
}

#[test]
fn env_block_id_only_no_cutoff_git_true() {
    let mut ctx = ctx_minimal();
    ctx.model_marketing_name = None;
    ctx.knowledge_cutoff = None;
    ctx.git_status = Some(orchestrator::prompt::GitStatus::default());
    let out = env_block::format(&ctx);
    assert!(out.contains("\n - Is a git repository: true\n"));
    // Bare-id model fallback, no cutoff bullet.
    assert!(out.contains("\n - You are powered by the model claude-opus-4-7.\n"));
    assert!(!out.contains("Assistant knowledge cutoff"));
    // The model line is immediately followed by the static "most recent" line.
    assert!(out.contains(
        "You are powered by the model claude-opus-4-7.\n - The most recent Claude models"
    ));
}
