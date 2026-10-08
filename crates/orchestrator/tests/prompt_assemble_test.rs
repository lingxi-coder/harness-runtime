//! System prompt sections compose through the production assembler.

use orchestrator::prompt::{assemble_system_prompt, FileTree, MemoryFile, SystemPromptContext};
use std::path::PathBuf;

fn ctx_minimal() -> SystemPromptContext {
    SystemPromptContext {
        cwd: PathBuf::from("/proj"),
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

// The verbatim preamble (claudemd.ts:89-90) that opened the OLD memory section.
const MEMORY_PREAMBLE: &str = "Codebase and user instructions are shown below.";

#[test]
fn minimal_assembly_no_memory_no_tools_no_footer() {
    let out = assemble_system_prompt(&ctx_minimal());
    // Must start with HEADER.
    assert!(out.starts_with("You are LingXi, an agentic command-line coding assistant."));
    // Must contain the `# Environment` block (R-P1a: replaces the old `<env>`).
    assert!(out.contains("\n\n# Environment\nYou have been invoked in the following environment: "));
    assert!(!out.contains("<env>"));
    // MUST NOT contain the memory section (R-P1c/d: LINGXI.md is a meta message
    // now), `<tools>`, or the `Notes:` FOOTER (R-P1b).
    assert!(!out.contains(MEMORY_PREAMBLE));
    assert!(!out.contains("Contents of "));
    assert!(!out.contains("<tools>"));
    assert!(!out.contains("Notes:"));
    // Context management follows the env block; the default act-don't-rederive
    // slot is the final 2.1.220 section.
    assert!(out.contains("# Context management"));
    assert!(out.ends_with("give a recommendation, not an exhaustive survey"));
    // The `# Memory` section is OMITTED when memory_dir is None (default) —
    // byte-identical to a build without the memory feature.
    assert!(!out.contains("# Memory\n"));
    assert!(!out.contains("You have a persistent file-based memory"));
}

#[test]
fn memory_section_emitted_when_memory_dir_set() {
    // The standard Claude profile receives the complete 2.1.220 auto-memory
    // protocol when the memory feature is active.
    let mut c = ctx_minimal();
    c.memory_dir = Some(PathBuf::from("/home/u/.lingxi/memdir"));
    let out = assemble_system_prompt(&c);
    assert!(out.contains(
        "# auto memory\n\nYou have a persistent, file-based memory system at `/home/u/.lingxi/memdir`. This directory already exists \u{2014} write to it directly with the Write tool"
    ));
    assert!(out.contains("`MEMORY.md` is always loaded into your conversation context"));
    assert!(out.contains("Anything already documented in LINGXI.md files."));
    // The 2.1.220 dynamic order is session guidance → memory → environment →
    // context management.
    let i_env = out.find("# Environment").expect("env");
    let i_mem = out.find("# auto memory\n").expect("memory");
    let i_ctx = out.find("# Context management").expect("ctx-mgmt");
    assert!(i_mem < i_env, "memory precedes the env block");
    assert!(i_mem < i_ctx, "memory precedes context management");
}

#[test]
fn memory_files_are_not_spliced_into_the_prompt() {
    // Even with LINGXI.md files present, the system prompt carries NO memory
    // section — the content moves to the additional-context meta message.
    let mut ctx = ctx_minimal();
    ctx.memory_files = vec![MemoryFile {
        parent: None,
        source_content: None,
        path: PathBuf::from("/proj/LINGXI.md"),
        body: "notes".into(),
        is_local_override: false,
        tier: memory::lingxi_md::LingxiMdTier::Project,
        globs: None,
        raw_content: "notes".into(),
        content_differs_from_disk: false,
    }];
    ctx.tool_names = vec!["Read".into(), "Write".into()];
    let out = assemble_system_prompt(&ctx);
    assert!(!out.contains(MEMORY_PREAMBLE));
    assert!(!out.contains("Contents of /proj/LINGXI.md"));
    assert!(!out.contains("<tools>"));
    assert!(!out.contains("Notes:"));
    assert!(out.ends_with("give a recommendation, not an exhaustive survey"));
}

#[test]
fn section_order_locked_header_body_env() {
    let out = assemble_system_prompt(&ctx_minimal());
    let i_header = out.find("You are LingXi").expect("header present");
    let i_body = out
        .find("You are an interactive agent that helps users with software engineering tasks.")
        .expect("static body present");
    let i_env = out.find("# Environment").expect("env present");
    assert!(i_header < i_body);
    assert!(i_body < i_env);
}

#[test]
fn double_lf_between_each_section() {
    let out = assemble_system_prompt(&ctx_minimal());
    // The cache splitter keeps HEADER as block 0 and the body as block 1.
    // Claude's body block itself begins with LF, so the assembled boundary is
    // the normal two-LF section separator plus that byte: exactly three LFs.
    let header_end = "You are LingXi, an agentic command-line coding assistant.";
    let after_header = &out[out.find(header_end).unwrap() + header_end.len()..];
    assert!(after_header.starts_with(
        "\n\n\nYou are an interactive agent that helps users with software engineering tasks."
    ));
    // The env section follows the body, on its own `\n\n` boundary, opening with
    // the `# Environment` heading.
    assert!(out.contains("\n\n# Environment\nYou have been invoked in the following environment: "));
}

#[test]
fn no_footer_on_main_prompt() {
    // R-P1b: the `Notes:` FOOTER is subagent-only (`H$t`). The main assembler
    // must not append it.
    let out = assemble_system_prompt(&ctx_minimal());
    assert!(!out.contains("Notes:"));
    assert!(!out.contains("not files you create."));
}

#[test]
fn footer_literal_still_exported_for_subagent() {
    // FOOTER is still exported (the subagent path uses it). Bullet 5 now carries
    // the 84-byte "(Files written as input to another tool…)" parenthetical to
    // match the live subagent trailer in `agent::handle` and the 2.1.193 binary.
    // Length locked at 900 bytes (5-bullet footer, two em-dashes + trailing LF).
    assert_eq!(orchestrator::prompt::locked_templates::FOOTER.len(), 900);
}

#[test]
fn static_body_sections_present_and_ordered_between_header_and_env() {
    // claude-code `J0` emits 6 static sections + 2 dynamic before the env block
    // (text output, session guidance), plus context management AFTER the env block.
    let mut ctx = ctx_minimal();
    ctx.is_interactive = true;
    ctx.tool_names = vec![
        "Read".into(),
        "Edit".into(),
        "Write".into(),
        "Glob".into(),
        "Grep".into(),
        "Bash".into(),
        "Agent".into(),
        "TodoWrite".into(),
    ];
    let out = assemble_system_prompt(&ctx);

    let i_header = out
        .find("You are LingXi, an agentic command-line coding assistant.")
        .unwrap();
    let i_open = out
        .find("You are an interactive agent that helps users with software engineering tasks.")
        .expect("Pym opening present");
    let i_zho = out
        .find("IMPORTANT: Assist with authorized security testing, defensive security, CTF challenges")
        .expect("zHo defensive-security guidance present in body");
    let i_urls = out
        .find("IMPORTANT: You must NEVER generate or guess URLs for the user")
        .expect("NEVER-URLs line present");
    let i_system = out
        .find("\n# System\n - All text you output outside of tool use")
        .unwrap();
    let i_doing = out
        .find("\n# Doing tasks\n - The user will primarily request you")
        .unwrap();
    let i_exec = out
        .find("\n# Executing actions with care\n\nCarefully consider the reversibility")
        .unwrap();
    // DIV-2: posix+Bash → Glob/Grep EXCLUDED from the dedicated list.
    let i_tools = out
        .find("\n# Using your tools\n - Prefer dedicated tools over Bash when one fits (Read, Edit, Write) \u{2014} reserve Bash for shell-only operations.")
        .expect("tools section with Read, Edit, Write (no Glob/Grep when Bash present)");
    let i_tone = out
        .find("\n# Tone and style\n - Only use emojis if the user explicitly requests it.")
        .unwrap();
    // GAP-1: # Text output present after # Tone and style, before env.
    let i_text_output = out
        .find("\n# Text output")
        .expect("text output section present");
    // GAP-3: # Session-specific guidance present (Agent tool present + interactive).
    let i_session = out
        .find("\n# Session-specific guidance")
        .expect("session guidance present");
    let i_env = out.find("\n# Environment").unwrap();
    // GAP-2: # Context management present AFTER env (binary cx() ordering).
    let i_ctx_mgmt = out
        .find("\n# Context management")
        .expect("context management present");

    assert!(i_header < i_open);
    assert!(i_open < i_zho);
    assert!(i_zho < i_urls);
    assert!(i_urls < i_system);
    assert!(i_system < i_doing);
    assert!(i_doing < i_exec);
    assert!(i_exec < i_tools);
    assert!(i_tools < i_tone);
    assert!(
        i_tone < i_text_output,
        "# Text output must follow # Tone and style"
    );
    assert!(
        i_text_output < i_session,
        "# Session-specific guidance must follow # Text output"
    );
    assert!(
        i_session < i_env,
        "# Session-specific guidance must precede the env block"
    );
    assert!(
        i_env < i_ctx_mgmt,
        "# Context management must follow the env block (binary cx() order)"
    );

    assert!(out.contains(" - Users may configure 'hooks', shell commands that execute"));
    assert!(out.contains("\n  - /help: Get help with using LingXi"));
    // Agent tool bullet in session guidance (2.1.232 default-ON fork text).
    assert!(out.contains("Calling Agent with subagent_type: \"fork\" creates a fork"));
    assert!(out.contains("suggest they type `! <command>`"));
}
