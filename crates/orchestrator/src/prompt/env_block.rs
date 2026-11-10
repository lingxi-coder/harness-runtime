//! Claude Code 2.1.319 environment, model-identity and catalog renderers.
//!
//! Stable catalog guidance and dynamic host/model announcements are separate
//! surfaces. `format` remains a combined compatibility entry point; new assembly
//! code uses the explicit renderers to preserve Native's cache/source grouping.
#![forbid(unsafe_code)]

use crate::prompt::SystemPromptContext;
use std::fmt::Write;

/// Native `tEo`; shared by initial environment announcements and later updates.
pub const UNTRUSTED_DOWNLOAD_GUIDANCE: &str = "Downloaded files and extracted archives are untrusted data: put each in its own new, empty directory, keep scripts you write in a different directory, and pass paths as arguments instead of running an interpreter or build tool from inside it. Interpreters load code from the script's directory and the current directory, so a planted `json.py` runs on `import json`. Run any Python that reads them with `-I`. This does not apply to code the user asked you to build or run.";
const MODEL_CATALOG_STANDARD: &str =
    include_str!("templates/claude-code-2.1.319/model-catalog-standard.md");
const MODEL_CATALOG_REWRITE: &str =
    include_str!("templates/claude-code-2.1.319/model-catalog-rewrite.md");
const PRODUCT_AVAILABILITY: &str =
    include_str!("templates/claude-code-2.1.319/product-availability.md");
const FAST_MODE_GUIDANCE: &str = "Fast mode for LingXi uses Claude Opus with faster output (it does not downgrade to a smaller model). It can be toggled with /fast.";

/// Format the `# Environment` block for a given context.
///
/// Returns a single `String` with NO trailing LF — the caller (the assembler)
/// appends a section separator. Each line carries the ` - ` bullet prefix per
/// claude-code `AG`.
#[must_use]
pub fn format_host_environment(ctx: &SystemPromptContext) -> String {
    format_host_environment_with_additional_dirs(ctx, &[])
}

/// Child agents can carry additional host-approved working directories.
#[must_use]
pub fn format_host_environment_with_additional_dirs(
    ctx: &SystemPromptContext,
    additional_dirs: &[String],
) -> String {
    let mut s = String::with_capacity(1024);

    // Header lines — byte-exact `Kym`. The second line has a TRAILING SPACE.
    s.push_str("# Environment\n");
    s.push_str("You have been invoked in the following environment: ");

    // Each subsequent element is one ` - ` bullet on its own line.
    // `cwd` uses display() — paths with non-UTF8 bytes get lossy-rendered
    // (claude-code is JS, always UTF-8).
    // Node's `process.cwd()` reports the physical path (for example macOS
    // `/var` resolves to `/private/var`). Canonicalize the already-existing
    // session cwd for the same observable value, while retaining the supplied
    // path if an unusual test/preview context is not materialized on disk.
    let physical_cwd = ctx.cwd.canonicalize().unwrap_or_else(|_| ctx.cwd.clone());
    write!(
        &mut s,
        "\n - Primary working directory: {}",
        physical_cwd.display()
    )
    .unwrap();

    // Worktree notice: present when `hf()!==null` (cwd is a git worktree).
    // Binary `c?"This is a git worktree — an isolated copy…":null` (offset 206671709).
    // Emitted between `Primary working directory:` and `Is a git repository:`.
    // The em-dash is U+2014.
    if ctx.in_worktree {
        s.push_str("\n - This is a git worktree \u{2014} an isolated copy of the repository. Run all commands from this directory. Do NOT `cd` to the original repository root.");
        // `_Jc` — the shared-git-stash bullet emitted right after the worktree
        // notice (present in 201 and 206; em-dash U+2014; ASCII apostrophes).
        // "other Claude sessions" -> "other LingXi sessions" (CLI-brand rebrand).
        s.push_str("\n - The git stash stack is shared with the main checkout and all other worktrees, and other LingXi sessions may push or pop it concurrently. Never use bare `git stash` / `git stash pop` \u{2014} you could pop another session's changes. Prefer a temporary WIP commit to set work aside; if you must stash, use `git stash push -u -m \"<unique-tag>\"`, immediately capture your entry's SHA via `git stash list --format='%H %gs'`, restore with `git stash apply <sha>` (not pop), and afterwards drop the entry, re-finding its current `stash@{n}` by tag first.");
    }

    // `Is a git repository: ${r}` — `r` is the boolean from `vy()`, rendered by
    // JS template interpolation as `true`/`false` (lowercase).
    let is_git = ctx.git_status.is_some();
    write!(&mut s, "\n - Is a git repository: {is_git}").unwrap();

    if !additional_dirs.is_empty() {
        s.push_str("\n - Additional working directories:");
        for directory in additional_dirs {
            write!(&mut s, "\n  - {directory}").unwrap();
        }
    }

    // `Platform: ${je.platform}` — `je.platform` is `process.platform`
    // (`darwin`/`linux`/`win32`). LingXi feeds `std::env::consts::OS`, which is
    // `macos`/`linux`/`windows`; the caller maps it to the node value.
    write!(&mut s, "\n - Platform: {}", ctx.platform).unwrap();

    // `tIo()` — already carries the `Shell: ` literal in its return value; the
    // caller collapses the raw $SHELL to zsh/bash/raw and stores just the value.
    write!(&mut s, "\n - Shell: {}", ctx.shell).unwrap();

    // `OS Version: ${o}` — `o = nIo()` = `${os.type()} ${os.release()}`.
    write!(&mut s, "\n - OS Version: {}", ctx.os_version).unwrap();

    write!(&mut s, "\n - {UNTRUSTED_DOWNLOAD_GUIDANCE}").unwrap();
    s
}

/// Native `iAt`; the model identity is an independent dynamic announcement.
#[must_use]
pub fn format_model_identity(ctx: &SystemPromptContext) -> String {
    format_model_identity_with_rewrite(ctx, false)
}

#[must_use]
pub fn format_model_identity_with_rewrite(ctx: &SystemPromptContext, rewrite: bool) -> String {
    let mut result = match (&ctx.model_marketing_name, rewrite) {
        (Some(name), false) => format!(
            "You are powered by the model named {name}. The exact model ID is {}.",
            ctx.model
        ),
        (Some(name), true) => format!(
            "You are currently powered by the model named {name}. The exact model ID is {}.",
            ctx.model
        ),
        (None, false) => format!("You are powered by the model {}.", ctx.model),
        (None, true) => format!("You are currently powered by the model {}.", ctx.model),
    };
    if let Some(cutoff) = &ctx.knowledge_cutoff {
        if rewrite {
            write!(&mut result, " Your innate base knowledge cutoff is {cutoff}. For anything which has happened after this date, you may have to use tools to help yourself get an accurate picture of the world.").unwrap();
        } else {
            write!(&mut result, " Assistant knowledge cutoff is {cutoff}.").unwrap();
        }
    }
    result
}

/// Native `jCr`: stable model catalog and product guidance, without host facts.
#[must_use]
pub fn format_catalog(ctx: &SystemPromptContext, is_subagent: bool, rewrite: bool) -> String {
    let is_claude = lingxi_core::host::model_capabilities::normalize_model_id(&ctx.model)
        .starts_with("claude-");
    let mut sections = vec!["# Environment"];
    if is_claude {
        sections.push(if rewrite {
            MODEL_CATALOG_REWRITE
        } else {
            MODEL_CATALOG_STANDARD
        });
    }
    sections.push(PRODUCT_AVAILABILITY);
    if is_claude && !is_subagent {
        sections.push(FAST_MODE_GUIDANCE);
    }
    let mut output = sections.remove(0).to_owned();
    for section in sections {
        write!(&mut output, "\n - {section}").unwrap();
    }
    output
}

/// Compatibility formatter for callers that still request a combined block.
#[must_use]
pub fn format(ctx: &SystemPromptContext) -> String {
    let mut output = format_host_environment(ctx);
    match &ctx.model_marketing_name {
        Some(name) => write!(
            &mut output,
            "\n - You are powered by the model named {name}. The exact model ID is {}.",
            ctx.model
        )
        .unwrap(),
        None => write!(
            &mut output,
            "\n - You are powered by the model {}.",
            ctx.model
        )
        .unwrap(),
    }
    if let Some(cutoff) = &ctx.knowledge_cutoff {
        write!(&mut output, "\n - Assistant knowledge cutoff is {cutoff}.").unwrap();
    }
    let catalog = format_catalog(ctx, false, false);
    output.push_str(catalog.strip_prefix("# Environment").unwrap());
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::FileTree;
    use std::path::PathBuf;

    fn ctx() -> SystemPromptContext {
        SystemPromptContext {
            native_system_turns: None,
            prompt_policy: None,
            cwd: PathBuf::from("/x"),
            platform: "linux".into(),
            model: "claude-opus-4-7".into(),
            model_marketing_name: None,
            knowledge_cutoff: None,
            shell: "bash".into(),
            os_version: "Linux 6.6".into(),
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
    fn env_block_id_only_model_no_cutoff() {
        let out = format(&ctx());
        assert!(
            out.starts_with("# Environment\nYou have been invoked in the following environment: ")
        );
        assert!(out.contains("\n - Primary working directory: /x"));
        assert!(out.contains("\n - Is a git repository: false"));
        assert!(out.contains("\n - Platform: linux"));
        assert!(out.contains("\n - Shell: bash"));
        assert!(out.contains("\n - OS Version: Linux 6.6"));
        assert!(out.contains("\n - You are powered by the model claude-opus-4-7."));
        assert!(!out.contains("Assistant knowledge cutoff"));
        // Static lines present, em-dash byte-exact.
        assert!(out.contains("Model IDs \u{2014} Fable 5.1: 'claude-fable-5-1'"));
        assert!(out.contains("Opus 5.5: 'claude-opus-5-5'"));
        assert!(out.ends_with("It can be toggled with /fast."));
    }

    #[test]
    fn context_319_environment_and_identity_match_native_renderers() {
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/context_prompts_319.json"
        ))
        .unwrap();
        for case in oracle["environment"].as_array().unwrap() {
            let input = &case["input"];
            let mut context = ctx();
            context.cwd = input["workingDirectory"].as_str().unwrap().into();
            context.platform = input["platform"].as_str().unwrap().into();
            context.shell = input["shell"].as_str().unwrap().into();
            context.os_version = input["osVersion"].as_str().unwrap().into();
            context.in_worktree = input["isWorktree"].as_bool().unwrap();
            context.git_status = input["isGitRepo"].as_bool().unwrap().then(Default::default);
            let additional_dirs = input["additionalWorkingDirectories"]
                .as_array()
                .unwrap()
                .iter()
                .map(|directory| directory.as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            let expected = case["expected"]
                .as_str()
                .unwrap()
                .replace("other Claude sessions", "other LingXi sessions");
            assert_eq!(
                format_host_environment_with_additional_dirs(&context, &additional_dirs),
                expected
            );
            if additional_dirs.is_empty() {
                assert_eq!(format_host_environment(&context), expected);
            }
        }
        for case in oracle["model_identity"].as_array().unwrap() {
            let input = &case["input"];
            let mut context = ctx();
            context.model = input["modelId"].as_str().unwrap().into();
            context.model_marketing_name = input["marketingName"].as_str().map(str::to_owned);
            context.knowledge_cutoff = input["knowledgeCutoff"].as_str().map(str::to_owned);
            assert_eq!(
                format_model_identity(&context),
                case["expected"].as_str().unwrap()
            );
            assert_eq!(
                format_model_identity_with_rewrite(&context, true),
                case["rewrite"].as_str().unwrap()
            );
        }
        // The upstream availability line describes Claude's product surfaces.
        // Replace only that complete line with the host's confirmed capability
        // statement; keep every other native catalog byte unchanged.
        let host_catalog = |native: &str| {
            const NATIVE_AVAILABILITY_LINE: &str = " - LingXi is available as a CLI in the terminal, desktop app (Mac/Windows), web app (claude.ai/code), and IDE extensions (VS Code, JetBrains).";
            assert_eq!(
                native
                    .split('\n')
                    .filter(|line| *line == NATIVE_AVAILABILITY_LINE)
                    .count(),
                1
            );
            native
                .split('\n')
                .map(|line| {
                    if line == NATIVE_AVAILABILITY_LINE {
                        format!(" - {PRODUCT_AVAILABILITY}")
                    } else {
                        line.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(
            format_catalog(&ctx(), false, false),
            host_catalog(oracle["model_catalog"]["body"].as_str().unwrap())
        );
        assert_eq!(
            format_catalog(&ctx(), true, false),
            host_catalog(oracle["model_catalog"]["subagent_body"].as_str().unwrap())
        );
        assert_eq!(
            format_catalog(&ctx(), false, true),
            host_catalog(oracle["model_catalog"]["rewrite_body"].as_str().unwrap())
        );
    }

    #[test]
    fn env_block_named_model_with_cutoff() {
        let mut c = ctx();
        c.git_status = Some(crate::prompt::GitStatus::default());
        c.model = "claude-opus-4-8[1m]".into();
        c.model_marketing_name = Some("Opus 4.8 (1M context)".into());
        c.knowledge_cutoff = Some("January 2026".into());
        let out = format(&c);
        assert!(out.contains("\n - Is a git repository: true"));
        assert!(out.contains(
            "\n - You are powered by the model named Opus 4.8 (1M context). \
The exact model ID is claude-opus-4-8[1m]."
        ));
        // Cutoff is a SEPARATE bullet immediately after the model line.
        assert!(
            out.contains("claude-opus-4-8[1m].\n - Assistant knowledge cutoff is January 2026.")
        );
    }

    #[test]
    fn non_claude_model_omits_the_claude_catalog_and_fast_mode_lines() {
        // Multi-provider divergence: a non-Claude model (deepseek) must NOT be
        // told the first-party Claude model catalog
        // (it misinforms + pollutes self-identity). The id-only model line and
        // the host capability statement still render.
        let mut c = ctx();
        c.model = "deepseek-v4-pro".into();
        c.model_marketing_name = None;
        let out = format(&c);
        assert!(
            out.contains("\n - You are powered by the model deepseek-v4-pro."),
            "id-only identity present: {out}"
        );
        assert!(
            !out.contains("The most recent Claude models"),
            "Claude catalog line must be omitted for a non-Claude model: {out}"
        );
        assert!(
            !out.contains("claude-fable-5-1"),
            "no claude-fable-5-1 in a deepseek prompt: {out}"
        );
        assert!(
            !out.contains("Fast mode for LingXi uses Claude Opus"),
            "Claude fast-mode note omitted for a non-Claude model: {out}"
        );
        // The host capability statement is the last bullet (fast-mode omitted).
        assert!(
            out.ends_with("LingXi runs in the current host client. Use the interfaces and capabilities explicitly exposed in this session."),
            "LingXi availability line remains, last: {out}"
        );
    }

    #[test]
    fn worktree_notice_emitted_when_in_worktree_true() {
        // DIV-1: `c?"This is a git worktree \u{2014} an isolated copy…":null`
        // (binary offset 206671709). When `in_worktree=true` the notice appears
        // between the `Primary working directory:` and `Is a git repository:` lines.
        let mut c = ctx();
        c.in_worktree = true;
        let out = format(&c);
        // Notice is present with the em-dash (U+2014).
        assert!(
            out.contains(
                "\n - This is a git worktree \u{2014} an isolated copy of the repository. \
Run all commands from this directory. Do NOT `cd` to the original repository root."
            ),
            "worktree notice missing"
        );
        // `_Jc` shared-stash bullet follows the worktree notice.
        assert!(
            out.contains(
                "\n - The git stash stack is shared with the main checkout and all other worktrees, \
and other LingXi sessions may push or pop it concurrently. Never use bare `git stash` / \
`git stash pop` \u{2014} you could pop another session's changes. Prefer a temporary WIP commit \
to set work aside; if you must stash, use `git stash push -u -m \"<unique-tag>\"`, immediately \
capture your entry's SHA via `git stash list --format='%H %gs'`, restore with `git stash apply \
<sha>` (not pop), and afterwards drop the entry, re-finding its current `stash@{n}` by tag first."
            ),
            "shared-stash bullet missing"
        );
        // Notice sits between the two fixed lines; the stash bullet follows it.
        let i_pwd = out.find("Primary working directory:").expect("pwd line");
        let i_notice = out.find("This is a git worktree").expect("notice present");
        let i_stash = out
            .find("The git stash stack is shared")
            .expect("stash bullet");
        let i_git = out.find("Is a git repository:").expect("git line");
        assert!(i_pwd < i_notice);
        assert!(i_notice < i_stash);
        assert!(i_stash < i_git);
    }

    #[test]
    fn worktree_notice_absent_when_in_worktree_false() {
        let c = ctx(); // in_worktree: false by default
        let out = format(&c);
        assert!(
            !out.contains("This is a git worktree"),
            "worktree notice must be absent when not in a worktree"
        );
        // The `Primary working directory:` and `Is a git repository:` lines are
        // still present and adjacent (no inserted line between them).
        assert!(out.contains("\n - Primary working directory: /x\n - Is a git repository: false"));
    }
}
