//! System prompt assembler for the model-specific Claude Code 2.1.220 prompt
//! profiles and LingXi's complete non-Claude harness.
//!
//! Entry point: [`assemble_system_prompt`].
#![forbid(unsafe_code)]

pub mod async_hook_response;
pub mod bash_output_note;
pub mod bg_session;
pub mod body_sections;
pub mod changed_files;
pub mod conditional_rules;
pub mod end_conversation;
pub mod env_block;
pub mod env_meta;
pub mod file_tree;
pub mod git_status;
pub mod goal_checkin;
pub mod goal_interruption;
pub mod locked_templates;
pub(crate) mod mcp_instructions;
pub mod memory_block;
pub mod memory_section;
pub mod memory_update;
pub mod mid_turn_input;
pub mod nested_memory;
pub mod plan_reminder;
pub mod sanitize;
pub mod silent_turn;
pub mod skill_listing;
pub mod subagent_env;
pub mod task_notification;
pub mod todo_reminder;
pub mod tool_search_reminder;
pub mod tools_block;
pub mod total_tokens;

pub mod large_memory;
pub use large_memory::{large_memory_warning_rows, shorten_memory_path};

pub use memory_block::{
    build_memdir_prefetch, build_session_memory_handle, real_provider, real_provider_with_excludes,
    MemoryHierarchyProvider, RealMemoryHierarchyProvider,
};

/// Re-export of the LINGXI.md tier enum so consumers that depend on
/// `orchestrator` (but not the `memory` crate directly) can name
/// [`MemoryFile::tier`] without an extra dependency.
pub use memory::lingxi_md::LingxiMdTier;

// `FOOTER` is intentionally NOT imported here: the MAIN assembler no longer
// appends it (R-P1b). It remains exported from `locked_templates` for the
// subagent path (`agent/handle.rs`) and is named fully-qualified in tests.
use crate::prompt::locked_templates::{HEADER, SECTION_SEP};
use std::path::PathBuf;

/// The active output-style section to inject into the system prompt.
///
/// Mirrors the inputs of TS `getOutputStyleSection`
/// (`claude-code/src/constants/prompts.ts:151-158`): `name` -> the
/// `# Output Style: <name>` heading, `prompt` -> the verbatim body that
/// follows on the next line.
///
/// Resolved by the caller — the engine `output_style` setting is fed through
/// [`outputstyles::resolve_builtin_output_style`], whose `name` / `prompt`
/// fields populate this borrow — and threaded into
/// [`assemble_system_prompt_with_style`]. The orchestrator itself does not
/// depend on the `outputstyles` crate; it only formats what it is handed.
#[derive(Debug, Clone, Copy)]
pub struct ActiveOutputStyle<'a> {
    /// Style name -> `# Output Style: <name>` heading text.
    pub name: &'a str,
    /// Verbatim style prompt body, emitted on the line after the heading.
    pub prompt: &'a str,
    /// `keepCodingInstructions` (defaults to `true`). When an active style sets
    /// this to `false`, the `# Doing tasks` (`Lym`) section is OMITTED from the
    /// system prompt — mirroring the binary gate
    /// `c===null||c.keepCodingInstructions===!0?Lym():null` (v2.1.185 offset
    /// 205821502).
    pub keep_coding_instructions: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptSectionScope {
    Shared,
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptSection {
    pub id: String,
    pub text: String,
    pub scope: PromptSectionScope,
    pub id_utf16_code_units: Option<Vec<u16>>,
    pub text_utf16_code_units: Option<Vec<u16>>,
}

pub(crate) struct RenderedPromptSections {
    pub text: String,
    /// Static source blocks captured before dynamic context is appended.
    pub source_sections: Vec<PromptSection>,
    /// Exact restored Native source vector when this prompt comes from resume.
    pub snapshot_system_prompt: Option<Vec<lingxi_llm_client::providers::anthropic::system_prompt::PromptText>>,
    /// Joined static bytes before the separately-carried dynamic context.
    pub static_text: String,
    /// Current Native `systemContext` block, kept separate from frozen source
    /// sections so provider projection can append it after every static block.
    pub dynamic_system_context: Option<String>,
}

pub(crate) struct PromptSectionSlot {
    pub id: String,
    pub text: Option<String>,
    pub scope: PromptSectionScope,
    pub id_utf16_code_units: Option<Vec<u16>>,
    pub text_utf16_code_units: Option<Vec<u16>>,
}

impl PromptSectionSlot {
    fn id_units(&self) -> Vec<u16> {
        self.id_utf16_code_units
            .clone()
            .unwrap_or_else(|| self.id.encode_utf16().collect())
    }

    pub(crate) fn id_prompt(
        &self,
    ) -> lingxi_llm_client::providers::anthropic::system_prompt::PromptText {
        self.id_utf16_code_units.as_ref().map_or_else(
            || {
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(
                    self.id.clone(),
                )
            },
            |units| {
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_utf16(
                    units.clone(),
                )
            },
        )
    }

    pub(crate) fn text_prompt(
        &self,
    ) -> Option<lingxi_llm_client::providers::anthropic::system_prompt::PromptText> {
        self.text.as_ref().map(|display| {
            self.text_utf16_code_units.as_ref().map_or_else(
                || {
                    lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(
                        display.clone(),
                    )
                },
                |units| {
                    lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_utf16(
                        units.clone(),
                    )
                },
            )
        })
    }

    pub(crate) fn public_id_prompt(
        &self,
    ) -> lingxi_llm_client::providers::anthropic::system_prompt::PromptText {
        lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_utf16(
            self.id_units()
                .into_iter()
                .take_while(|unit| *unit != b':' as u16)
                .collect(),
        )
    }
}

pub(crate) fn prompt_section_slots(
    sections: Vec<PromptSection>,
    ctx: &SystemPromptContext,
) -> Vec<PromptSectionSlot> {
    let lean = sections.iter().any(|section| section.id_equals_ascii("lean_body"));
    let suffix = if lean { ":L" } else { "" };
    let mut shared = Vec::new();
    let mut available = Vec::new();
    for section in sections {
        if section.scope == PromptSectionScope::Shared {
            shared.push(PromptSectionSlot {
                id: section.id,
                text: Some(section.text),
                scope: PromptSectionScope::Shared,
                id_utf16_code_units: section.id_utf16_code_units,
                text_utf16_code_units: section.text_utf16_code_units,
            });
        } else {
            available.push(section);
        }
    }
    let mut output_style = available
        .iter()
        .position(|section| section.id_equals_ascii("output_style"))
        .map(|index| available.remove(index));
    let mut slots = shared;
    let ordered = [
        "communication",
        "pronouns",
        "action_caution",
        "task_continuity",
        "fable_identity",
        "tool_param_json",
        "session_guidance",
        "memory",
        "env_info",
        "bg-session",
        "context_management",
        "brief",
        "focus_mode",
        "act_dont_rederive",
        "delivering_work_max",
        "overcorrection",
        "subagent_steer_delegation",
        "opus5_reduced_delegation",
        "heron_brook",
        "brook_heron",
        "willow_tern",
        "autonomy_append",
        "endconv_deferred_hint",
    ];
    for base in ordered {
        if base == "memory"
            && ctx.exclude_dynamic_sections
            && !available
                .iter()
                .any(|section| section.id_base_equals_ascii("memory"))
        {
            continue;
        }
        let base = if base == "env_info" {
            if ctx.exclude_dynamic_sections {
                "env_info_static"
            } else {
                "env_info_simple"
            }
        } else {
            base
        };
        let slot = if let Some(index) = available
            .iter()
            .position(|section| section.id_base_equals_ascii(base))
        {
            let section = available.remove(index);
            PromptSectionSlot {
                id: section.id,
                text: Some(section.text),
                scope: PromptSectionScope::Session,
                id_utf16_code_units: section.id_utf16_code_units,
                text_utf16_code_units: section.text_utf16_code_units,
            }
        } else {
            let id = match base {
                "communication" | "action_caution" | "memory" | "focus_mode" => {
                    format!("{base}{suffix}")
                }
                "session_guidance" => format!(
                    "session_guidance{suffix}{}:false",
                    if ctx.exclude_dynamic_sections {
                        ":sdk"
                    } else {
                        ""
                    }
                ),
                _ => base.to_string(),
            };
            PromptSectionSlot {
                id,
                text: None,
                scope: PromptSectionScope::Session,
                id_utf16_code_units: None,
                text_utf16_code_units: None,
            }
        };
        slots.push(slot);
        if matches!(base, "env_info_simple" | "env_info_static") {
            if let Some(section) = output_style.take() {
                slots.push(PromptSectionSlot {
                    id: section.id,
                    text: Some(section.text),
                    scope: PromptSectionScope::Session,
                    id_utf16_code_units: section.id_utf16_code_units,
                    text_utf16_code_units: section.text_utf16_code_units,
                });
            }
        }
    }
    slots.extend(available.into_iter().map(|section| PromptSectionSlot {
        id: section.id,
        text: Some(section.text),
        scope: PromptSectionScope::Session,
        id_utf16_code_units: section.id_utf16_code_units,
        text_utf16_code_units: section.text_utf16_code_units,
    }));
    slots
}

impl PromptSection {
    fn new(id: &str, text: String, scope: PromptSectionScope) -> Self {
        Self {
            id: id.to_string(),
            text,
            scope,
            id_utf16_code_units: None,
            text_utf16_code_units: None,
        }
    }

    fn id_units(&self) -> Vec<u16> {
        self.id_utf16_code_units
            .clone()
            .unwrap_or_else(|| self.id.encode_utf16().collect())
    }

    pub(crate) fn text_prompt(
        &self,
    ) -> lingxi_llm_client::providers::anthropic::system_prompt::PromptText {
        self.text_utf16_code_units.as_ref().map_or_else(
            || {
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(
                    self.text.clone(),
                )
            },
            |units| {
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_utf16(
                    units.clone(),
                )
            },
        )
    }

    pub(crate) fn id_prompt(
        &self,
    ) -> lingxi_llm_client::providers::anthropic::system_prompt::PromptText {
        self.id_utf16_code_units.as_ref().map_or_else(
            || {
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(
                    self.id.clone(),
                )
            },
            |units| {
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_utf16(
                    units.clone(),
                )
            },
        )
    }

    fn id_equals_ascii(&self, value: &str) -> bool {
        self.id_units().into_iter().eq(value.encode_utf16())
    }

    fn id_base_units(&self) -> Vec<u16> {
        self.id_units()
            .into_iter()
            .take_while(|unit| *unit != b':' as u16)
            .collect()
    }

    fn id_base_equals_ascii(&self, value: &str) -> bool {
        self.id_base_units().into_iter().eq(value.encode_utf16())
    }

    pub(crate) fn public_id_prompt(
        &self,
    ) -> lingxi_llm_client::providers::anthropic::system_prompt::PromptText {
        lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_utf16(
            self.id_base_units(),
        )
    }
}

pub(crate) fn render_prompt_sections(sections: &[PromptSection]) -> String {
    render_prompt_sections_scoped(sections).text
}

pub(crate) fn render_prompt_sections_scoped(sections: &[PromptSection]) -> RenderedPromptSections {
    let mut text = String::with_capacity(2048);
    let mut session_started = false;
    for section in sections {
        debug_assert!(!section.id.is_empty());
        if section.scope == PromptSectionScope::Session {
            session_started = true;
        } else {
            debug_assert!(
                !session_started,
                "shared prompt section after session section"
            );
        }
        if !text.is_empty() {
            push_section_separator(&mut text);
        }
        text.push_str(&section.text);
    }
    RenderedPromptSections {
        static_text: text.clone(),
        text,
        source_sections: sections.to_vec(),
        snapshot_system_prompt: None,
        dynamic_system_context: None,
    }
}

/// Assemble a system prompt from a [`SystemPromptContext`].
///
/// Equivalent to [`assemble_system_prompt_with_style`] with no active style —
/// i.e. the default/no-output-style path. The produced bytes are LOCKED; see
/// that function for the full section order.
#[must_use]
pub fn assemble_system_prompt(ctx: &SystemPromptContext) -> String {
    assemble_system_prompt_with_style(ctx, None)
}

/// Assemble a system prompt, optionally injecting an active output-style
/// section.
///
/// Section order is locked to the clean-room Claude Code 2.1.220 oracle:
///
/// 1. `HEADER`
/// 2. model-selected body and session guidance
/// 3. optional file-memory protocol
/// 4. optional `# Environment` block
/// 5. optional output style and background-session sections
/// 6. context management and model-specific post-context sections
///
/// LINGXI.md content remains an additional-context meta message rather than a
/// system-prompt section. Tools travel in the wire `tools` array, and the
/// `Notes:` footer remains subagent-only.
///
/// Separator between sections is exactly `\n\n` (one blank line).
///
/// OUTSTYLE.2: the output-style section is a `getSystemPrompt` body section
/// (`constants/prompts.ts:505-507`). When `output_style` is `None` (the
/// `'default'` / unset path) the section is skipped entirely.
#[must_use]
pub fn assemble_system_prompt_with_style(
    ctx: &SystemPromptContext,
    output_style: Option<ActiveOutputStyle<'_>>,
) -> String {
    render_prompt_sections(&named_prompt_sections_with_style(ctx, output_style))
}

/// Preserve the section boundaries used by the prompt renderer so Mod hooks
/// can operate on named entries before the model request is serialized.
#[must_use]
pub(crate) fn named_prompt_sections_with_style(
    ctx: &SystemPromptContext,
    output_style: Option<ActiveOutputStyle<'_>>,
) -> Vec<PromptSection> {
    let mut sections = vec![PromptSection::new(
        "header",
        HEADER.to_string(),
        PromptSectionScope::Shared,
    )];

    // Static system-prompt BODY (claude-code `J0` statics: opening / `# System`
    // / `# Doing tasks` / `# Executing actions with care` / `# Using your tools`
    // / `# Tone and style`), spliced between the HEADER and the env block. In
    // the binary these six statics precede the dynamic env/memory group, so
    // emitting them here keeps the statics-before-env relative order. The
    // `Pym` opening clause toggles on whether an output style is active; the
    // `# Doing tasks` (`Lym`) section is gated on the active style's
    // `keepCodingInstructions` (default true ⇒ no-style and builtins stay
    // byte-identical to before).
    let keep_coding = output_style.is_none_or(|s| s.keep_coding_instructions);
    // Session guidance differs between the interactive TUI and print/SDK
    // paths. Keep that signal explicit in the context rather than assuming
    // every caller is interactive.
    let has_agent = ctx.tool_names.iter().any(|t| t == "Agent");
    // 2.1.232 `SPe()`: fork session-guidance is ON by default for interactive
    // main-thread prompts. Coordinator sessions use a separate prompt builder.
    let fork_mode = lingxi_core::host::fork_subagent::is_fork_subagent_enabled(
        /* is_coordinator */ false,
        /* is_non_interactive */ !ctx.is_interactive,
    );
    sections.extend(body_sections::named_sections(
        output_style.is_some(),
        keep_coding,
        &ctx.tool_names,
        /* is_interactive = */ ctx.is_interactive,
        /* has_agent_tool = */ has_agent,
        /* fork_mode_enabled = */ fork_mode,
        /* model = */ &ctx.model,
        /* skills_available = */ ctx.skills_available,
        /* exclude_dynamic_sections = */ ctx.exclude_dynamic_sections,
    ));

    // R-P1c/R-P1d: the LINGXI.md memory block is NO LONGER spliced into the
    // MAIN system prompt. claude-code v2.1.183 carries it as an additional-
    // context `<system-reminder>` meta user message (the `instructions` key of
    // `A6n(re, userContext)`), prepended to each turn's messages — NOT a system-
    // prompt section. The orchestrator builds that message from
    // `memory_block::format` (see `conversation.rs::additional_context_message`).
    // `ctx.memory_files` is retained on the context for that path / callers.

    // NO `<tools>` block: claude-code passes tools to the model via the wire
    // `tools:` API array, NOT a system-prompt text list (`<tools>` / `</tools>`
    // are 0 hits in the v2.1.181 binary). `ctx.tool_names` is kept on the context
    // for callers but is no longer rendered into the prompt.

    // File-based-memory WRITE instructions. The pinned 2.1.220 order is
    // session guidance → memory → environment. Lean Claude profiles use the
    // compact `# Memory` protocol; standard Claude and non-Claude FullHarness
    // profiles keep the complete `# auto memory` protocol.
    if let Some(dir) = &ctx.memory_dir {
        let memory_id = if sections.iter().any(|section| section.id == "lean_body") {
            "memory:L"
        } else {
            "memory"
        };
        sections.push(PromptSection::new(
            memory_id,
            memory_section::render_for_profile(
                &dir.to_string_lossy(),
                lingxi_core::host::model_capabilities::prompt_profile_for(&ctx.model),
            ),
            PromptSectionScope::Session,
        ));
    }

    // `--exclude-dynamic-system-prompt-sections`: OMIT the per-machine env
    // block (cwd / env / git / OS / shell) from the system prompt so the static
    // prompt is identical across machines (prompt-cache reuse). The conversation
    // re-emits the same env block in the first-user-message context reminder.
    if !ctx.exclude_dynamic_sections {
        sections.push(PromptSection::new(
            "env_info_simple",
            env_block::format(ctx),
            PromptSectionScope::Session,
        ));
    }

    if let Some(style) = output_style {
        sections.push(PromptSection::new(
            "output_style",
            output_style_section(style),
            PromptSectionScope::Session,
        ));
    }

    // (M8 cc2.1.198) `# Background Session` (`_ff()` @219583413) — bg jobs
    // only (`LINGXI_SESSION_KIND=bg` + `LINGXI_JOB_DIR`); binary position is
    // after output style and before context management.
    if let Some(bg) = bg_session::from_env() {
        sections.push(PromptSection::new(
            "bg-session",
            bg,
            PromptSectionScope::Session,
        ));
    }

    // GAP-2: `# Context management` (iIm) — always, unconditional.
    // Binary cx() position: after env_info_simple + language + output_style +
    // bg-session + scratchpad. Model-specific post-context sections may follow.
    sections.push(PromptSection::new(
        "context_management",
        body_sections::CONTEXT_MANAGEMENT_SECTION.to_string(),
        PromptSectionScope::Session,
    ));

    // Model-specific 2.1.220 tail: act-don't-rederive for every profile,
    // Opus-5 delivery/correction restrictions, or Fable/Mythos autonomous
    // mitigation. These are after context management in the oracle.
    sections.extend(body_sections::post_context_named_sections(
        &ctx.model,
        output_style.is_some(),
    ));

    // R-P1b: NO `Notes:` FOOTER on the MAIN prompt.
    sections
}

/// Format the active output-style section, byte-for-byte as TS
/// `getOutputStyleSection` (`constants/prompts.ts:156-157`):
/// `# Output Style: {name}\n{prompt}`.
#[must_use]
fn output_style_section(style: ActiveOutputStyle<'_>) -> String {
    format!("# Output Style: {}\n{}", style.name, style.prompt)
}

/// Append a section separator that produces exactly one blank line
/// between two adjacent sections, regardless of whether the previous
/// section already ended with a single `\n` (`tools_block`, `env_block`'s
/// cutoff line) or not (`HEADER`, and `memory_block`, which after the GAP-3
/// rewrite ends with the last file's trimmed body — no trailing `\n`). One
/// blank line = two LFs total at the boundary.
fn push_section_separator(s: &mut String) {
    if s.ends_with('\n') {
        s.push('\n');
    } else {
        s.push_str(SECTION_SEP);
    }
}

/// Runtime context required to assemble a system prompt.
///
/// Constructed by `ConversationOrchestrator::run_turn` once per turn
/// (cheap to clone where needed; not persisted). All I/O has already
/// happened by the time the assembler sees this — the assembler itself
/// is purely synchronous string concatenation.
///
/// Field order is LOCKED for `Debug` output stability — see M5-03 plan
/// "Critical 1:1 fidelity items".
#[derive(Debug, Clone)]
pub struct SystemPromptContext {
    /// Current working directory — emitted as the first `<env>` line.
    pub cwd: PathBuf,
    /// `std::env::consts::OS` (`macos` / `linux` / `windows` …).
    pub platform: String,
    /// Model ID actually being sent to the API (`claude-opus-4-7` …).
    pub model: String,
    /// Friendly marketing name, when known (`Opus 4.7`). When `None`,
    /// the env block falls back to the ID-only sentence.
    pub model_marketing_name: Option<String>,
    /// Knowledge cutoff text (e.g. `"January 2026"`). When `None`,
    /// the env block omits the cutoff line.
    pub knowledge_cutoff: Option<String>,
    /// Shell name (`zsh` / `bash` / …). Probed from `$SHELL` at the
    /// call site; the assembler does not re-probe.
    pub shell: String,
    /// `uname -sr` value (`Darwin 25.3.0` …).
    pub os_version: String,
    /// `Some(_)` when cwd is inside a git repo; otherwise `None`.
    pub git_status: Option<GitStatus>,
    /// `true` when the cwd is inside a git worktree (`hf()!==null` in
    /// claude-code). When true the env block emits the worktree notice
    /// ("This is a git worktree — an isolated copy …") between the
    /// `Primary working directory:` and `Is a git repository:` lines.
    pub in_worktree: bool,
    /// Direct + once-recursive children of cwd (depth ≤ 2).
    pub file_tree: FileTree,
    /// LINGXI.md hierarchy — already in claude-code splice order
    /// (managed → home → repo → repo-local override), each tagged with its
    /// [`MemoryFile::tier`].
    pub memory_files: Vec<MemoryFile>,
    /// Available tool names — alphabetic order. Sorting happens here,
    /// NOT in `tools_block::format`.
    pub tool_names: Vec<String>,
    /// Whether at least one user-invocable (model-invocable) skill exists —
    /// claude-code `nz()`/skill-list non-empty. Gates the `# Session-specific
    /// guidance` Skill-invocation bullet (with the Skill tool present).
    pub skills_available: bool,
    /// Whether this is the interactive TUI path. Print/SDK calls set this false,
    /// which suppresses the `! <command>` session-guidance bullet exactly as in
    /// Claude Code's non-interactive prompt.
    pub is_interactive: bool,
    /// Resolved user-memdir path when the file-based memory feature is active
    /// (the memory prefetch is wired — claude-code `tengu_moth_copse`, default
    /// OFF). `Some(path)` emits the `# Memory` write-instructions section;
    /// `None` (the default) omits it — byte-identical to a build without memory.
    pub memory_dir: Option<PathBuf>,
    /// CLI `--exclude-dynamic-system-prompt-sections`. When `true`, the
    /// per-machine `env_block` is OMITTED from the assembled system prompt (it
    /// is emitted in the first-user-message context reminder instead). `false`
    /// (the default) keeps the env block in the prompt — byte-identical to
    /// before this field existed.
    pub exclude_dynamic_sections: bool,
}

/// Result of a git-status probe over the cwd.
///
/// `Some(GitStatus)` is returned by [`crate::prompt::git_status::probe`]
/// when the cwd is inside a git repo. The fields are PII-safe (no file
/// content, only summary).
#[derive(Debug, Clone, Default)]
pub struct GitStatus {
    /// Current branch name (`main`, `feat/x`). `HEAD detached` when in
    /// a detached state. NEVER empty.
    pub branch: String,
    /// `true` when both index and working tree are unmodified.
    pub working_dir_clean: bool,
    /// Output of `git diff --stat --cached` + `git diff --stat`,
    /// joined by `\n`, truncated at 4 KB. Empty when clean.
    pub file_changes_summary: String,
}

/// Snapshot of the file tree at depth ≤ 2 under cwd.
#[derive(Debug, Clone, Default)]
pub struct FileTree {
    /// Entries in display order (dirs before files at each level,
    /// alphabetic within). Populated by
    /// [`crate::prompt::file_tree::probe`].
    pub entries: Vec<FileTreeEntry>,
}

/// One entry in [`FileTree`].
#[derive(Debug, Clone)]
pub struct FileTreeEntry {
    /// Absolute path on disk.
    pub path: PathBuf,
    /// `true` when this is a directory; `false` when a regular file.
    pub is_dir: bool,
    /// `0` = direct child of cwd; `1` = grandchild. Never above 1
    /// for this assembler (depth limit = 2 means 0..=1).
    pub depth: u8,
}

/// One loaded LINGXI.md (or `LINGXI.local.md`) file.
///
/// Distinct from [`memory::lingxi_md::LoadedFile`] — the
/// assembler retains the native sanitized acquisition body.
#[derive(Debug, Clone)]
pub struct MemoryFile {
    /// Absolute path on disk (PII-safe — only emitted into the
    /// system prompt, never into telemetry).
    pub path: PathBuf,
    /// Direct instruction file which imported this file, when present.
    pub parent: Option<PathBuf>,
    /// Sanitized source content exposed to Mod instruction lists.
    pub source_content: Option<String>,
    /// Sanitized file body with its original leading/trailing whitespace.
    /// Whitespace-only files are filtered by the acquisition loader.
    pub body: String,
    /// `true` when this is a `LINGXI.local.md`; `false` for `LINGXI.md`.
    ///
    /// Records direct local-file origin; an imported child keeps its inherited
    /// tier while this flag distinguishes it from the named local entry.
    pub is_local_override: bool,
    /// Which LINGXI.md tier the file came from. Selects the injection
    /// description (`getLingxiMds`, claudemd.ts:1168-1186): Managed and User
    /// share the global-instructions wording; Project and Local each have
    /// their own.
    pub tier: memory::lingxi_md::LingxiMdTier,
    /// `paths:` frontmatter globs, when the file is a CONDITIONAL rule
    /// (`parseFrontmatterPaths`, claudemd.ts:254-279). `None` for an
    /// unconditional file. Conditional rules are NOT eagerly injected into the
    /// system prompt (claudemd.ts:773 `conditionalRule:false` filter — enforced
    /// by [`memory_block::format`]); instead they are lazily activated per
    /// edited/opened file by the orchestrator's
    /// `nested_memory_reminder_messages` (§F, claudemd.ts
    /// `processConditionedMdRules`).
    pub globs: Option<Vec<String>>,
    /// claude-code `rawContent` — the file's RAW on-disk text, byte-verbatim
    /// (carried through from [`memory::lingxi_md::loader::MemoryEntry::raw_content`]).
    ///
    /// Distinct from [`Self::body`], which is stripped. Consumed by
    /// `ConversationOrchestrator::seed_memory_read_state` so a later `Read` of
    /// this path can be compared against the bytes actually on disk.
    pub raw_content: String,
    /// claude-code `contentDiffersFromDisk` (`bn_` @230803364: `p = d !== e`,
    /// an exact compare with NO trim) — whether frontmatter / HTML-comment
    /// stripping changed the body relative to disk.
    ///
    /// Becomes the seeded entry's `is_partial_view`.
    pub content_differs_from_disk: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_constructs_from_minimal_inputs() {
        let ctx = SystemPromptContext {
            cwd: PathBuf::from("/tmp"),
            platform: "macos".into(),
            model: "claude-opus-4-7".into(),
            model_marketing_name: Some("Opus 4.7".into()),
            knowledge_cutoff: Some("January 2026".into()),
            shell: "zsh".into(),
            os_version: "Darwin 25.3.0".into(),
            git_status: None,
            in_worktree: false,
            file_tree: FileTree {
                entries: Vec::new(),
            },
            memory_files: Vec::new(),
            tool_names: Vec::new(),
            skills_available: false,
            is_interactive: false,
            memory_dir: None,
            exclude_dynamic_sections: false,
        };
        assert_eq!(ctx.cwd, PathBuf::from("/tmp"));
        assert_eq!(ctx.model, "claude-opus-4-7");
        assert!(ctx.git_status.is_none());
    }

    #[test]
    fn git_status_default_is_empty_unclean_branch_blank() {
        let g = GitStatus::default();
        assert_eq!(g.branch, "");
        assert!(!g.working_dir_clean);
        assert_eq!(g.file_changes_summary, "");
    }

    #[test]
    fn file_tree_default_is_empty() {
        let t = FileTree::default();
        assert!(t.entries.is_empty());
    }

    #[test]
    fn memory_file_constructs() {
        let f = MemoryFile {
            parent: None,
            source_content: None,
            path: PathBuf::from("/proj/LINGXI.md"),
            body: "# title\nbody\n".into(),
            is_local_override: false,
            tier: memory::lingxi_md::LingxiMdTier::Project,
            globs: None,
            raw_content: "# title\nbody\n".into(),
            content_differs_from_disk: false,
        };
        assert!(!f.is_local_override);
        assert!(f.body.contains("title"));
    }

    // ---- OUTSTYLE.2: output-style section injection ----

    fn ctx_minimal() -> SystemPromptContext {
        SystemPromptContext {
            cwd: PathBuf::from("/proj"),
            platform: "macos".into(),
            model: "claude-opus-4-7".into(),
            model_marketing_name: Some("Opus 4.7".into()),
            knowledge_cutoff: Some("January 2026".into()),
            shell: "zsh".into(),
            os_version: "Darwin 25.3.0".into(),
            git_status: None,
            in_worktree: false,
            file_tree: FileTree::default(),
            memory_files: Vec::new(),
            tool_names: vec!["Read".into(), "Write".into()],
            skills_available: false,
            is_interactive: false,
            memory_dir: None,
            exclude_dynamic_sections: false,
        }
    }

    #[test]
    fn exclude_dynamic_sections_omits_env_block() {
        // `--exclude-dynamic-system-prompt-sections`: the per-machine env block
        // (whose signature line is "Primary working directory:") is in the
        // system prompt by default, and OMITTED when the flag is set (the
        // conversation re-emits it in the first user message instead).
        let mut ctx = ctx_minimal();
        assert!(
            assemble_system_prompt(&ctx).contains("Primary working directory"),
            "env block must be present by default"
        );
        ctx.exclude_dynamic_sections = true;
        assert!(
            !assemble_system_prompt(&ctx).contains("Primary working directory"),
            "env block must be OMITTED when exclude_dynamic_sections is set"
        );
    }

    #[test]
    fn no_style_is_byte_identical_to_default_path() {
        let ctx = ctx_minimal();
        // The public entry point and the explicit `None` style must match,
        // and neither may contain an output-style heading.
        let default = assemble_system_prompt(&ctx);
        let none = assemble_system_prompt_with_style(&ctx, None);
        assert_eq!(default, none);
        assert!(!default.contains("# Output Style:"));
        // Spot-check the locked envelope: opens with HEADER and ends with the
        // post-context act-don't-rederive slot (no `Notes:` footer).
        assert!(default.starts_with("You are LingXi, an agentic command-line coding assistant."));
        assert!(!default.contains("Notes:"));
        assert!(default.ends_with("give a recommendation, not an exhaustive survey"));
    }

    #[test]
    fn named_prompt_sections_keep_stable_order_and_cache_scopes() {
        let sections = named_prompt_sections_with_style(&ctx_minimal(), None);
        let ids = sections
            .iter()
            .map(|section| section.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            &ids[..7],
            &[
                "header",
                "intro",
                "system",
                "doing_tasks",
                "actions",
                "tools",
                "tone"
            ]
        );
        assert!(ids.contains(&"communication"));
        assert!(ids.contains(&"env_info_simple"));
        assert!(ids.contains(&"context_management"));
        let mut unique = std::collections::HashSet::new();
        assert!(ids.iter().all(|id| unique.insert(*id)));
        let first_session = sections
            .iter()
            .position(|section| section.scope == PromptSectionScope::Session)
            .unwrap();
        assert!(sections[..first_session]
            .iter()
            .all(|section| section.scope == PromptSectionScope::Shared));
        assert!(sections[first_session..]
            .iter()
            .all(|section| section.scope == PromptSectionScope::Session));
    }

    #[test]
    fn scoped_rendering_retains_native_source_sections() {
        let shared = PromptSection::new("shared", "共享".into(), PromptSectionScope::Shared);
        let session = PromptSection::new("session", "会话".into(), PromptSectionScope::Session);
        let rendered = render_prompt_sections_scoped(&[shared.clone(), session]);
        assert_eq!(rendered.text, "共享\n\n会话");
        assert_eq!(rendered.static_text, rendered.text);
        assert_eq!(rendered.source_sections.len(), 2);
        assert_eq!(
            rendered.source_sections[0].scope,
            PromptSectionScope::Shared
        );
        assert_eq!(
            rendered.source_sections[1].scope,
            PromptSectionScope::Session
        );
        assert!(rendered.snapshot_system_prompt.is_none());

        let shared_only = render_prompt_sections_scoped(&[shared]);
        assert_eq!(shared_only.source_sections.len(), 1);
    }

    #[test]
    fn lean_sdk_section_names_follow_the_oracle_suffixes() {
        let mut ctx = ctx_minimal();
        ctx.model = "claude-opus-5".into();
        ctx.tool_names.push("Agent".into());
        ctx.is_interactive = true;
        ctx.exclude_dynamic_sections = true;
        let sections = named_prompt_sections_with_style(&ctx, None);
        let ids = sections
            .iter()
            .map(|section| section.id.as_str())
            .collect::<Vec<_>>();
        assert!(ids.contains(&"lean_body"));
        assert!(ids.contains(&"communication:L"));
        assert!(ids.contains(&"action_caution:L"));
        assert!(ids.contains(&"session_guidance:L:sdk:false"));
    }

    #[test]
    fn optional_prompt_slots_preserve_present_section_bytes() {
        let standard = ctx_minimal();
        let mut lean = standard.clone();
        lean.model = "claude-opus-5".into();
        let mut sdk = lean.clone();
        sdk.exclude_dynamic_sections = true;
        let mut sdk_with_memory = sdk.clone();
        sdk_with_memory.memory_dir = Some(PathBuf::from("/tmp/memory"));
        for ctx in [standard, lean, sdk, sdk_with_memory] {
            let sections = named_prompt_sections_with_style(&ctx, None);
            let expected = render_prompt_sections(&sections);
            let slots = prompt_section_slots(sections, &ctx);
            assert!(slots
                .iter()
                .any(|slot| slot.id == "task_continuity" && slot.text.is_none()));
            assert!(slots
                .iter()
                .any(|slot| slot.id == "brief" && slot.text.is_none()));
            let present = slots
                .into_iter()
                .filter_map(|slot| {
                    slot.text.map(|text| PromptSection {
                        id: slot.id,
                        text,
                        scope: slot.scope,
                        id_utf16_code_units: slot.id_utf16_code_units,
                        text_utf16_code_units: slot.text_utf16_code_units,
                    })
                })
                .collect::<Vec<_>>();
            assert_eq!(render_prompt_sections(&present), expected);
        }
    }

    #[test]
    fn active_style_injects_section_after_env_no_footer() {
        let ctx = ctx_minimal();
        let style = ActiveOutputStyle {
            name: "Explanatory",
            prompt: "BODY LINE 1\nBODY LINE 2",
            keep_coding_instructions: true,
        };
        let out = assemble_system_prompt_with_style(&ctx, Some(style));

        // Heading + body are present, exactly per getOutputStyleSection.
        assert!(out.contains("# Output Style: Explanatory\nBODY LINE 1\nBODY LINE 2"));

        // Placement: after the `# Environment` block, on a `\n\n` boundary, and
        // it is the LAST section (no `<tools>` block, no `Notes:` FOOTER). The
        // prompt now ends with the style body.
        assert!(!out.contains("<tools>"));
        assert!(!out.contains("Notes:"));
        let i_env = out.find("# Environment").expect("env present");
        let i_style = out.find("# Output Style:").expect("style present");
        assert!(i_env < i_style, "env must come before style");
        assert!(i_env < i_style, "env must come before style");
        // GAP-2: context management is now the LAST section, after output-style.
        let i_ctx = out
            .find("# Context management")
            .expect("context management present");
        assert!(
            i_style < i_ctx,
            "context management must come AFTER output-style"
        );
        // An explicit style suppresses the default act-don't-rederive slot.
        // Opus 4.7 has no other model tail, so context management is terminal.
        assert!(out.ends_with("you don't need to wrap up early or hand off mid-task."));
    }

    #[test]
    fn active_style_injects_after_env_when_no_memory_or_tools() {
        let mut ctx = ctx_minimal();
        ctx.tool_names = Vec::new();
        ctx.memory_files = Vec::new();
        let style = ActiveOutputStyle {
            name: "Learning",
            prompt: "P",
            keep_coding_instructions: true,
        };
        let out = assemble_system_prompt_with_style(&ctx, Some(style));
        // No memory section (LINGXI.md is a meta message now) and no tools section.
        assert!(!out.contains("Codebase and user instructions are shown below."));
        assert!(!out.contains("<tools>"));
        // The style is NOT the last section — context management follows.
        assert!(out.contains("# Output Style: Learning\nP"));
        assert!(out.ends_with("you don't need to wrap up early or hand off mid-task."));
    }

    // ---- SDK-owned system-prompt cache projection ----

    #[test]
    fn assembled_default_prompt_projects_through_current_source_vector_api() {
        // The current API carries source sections, not a flattened string.
        // With no provider-global cache scope, the SDK joins the source vector
        // into one ordinary Anthropic system block without changing its text.
        let ctx = ctx_minimal();
        let s = assemble_system_prompt(&ctx);
        let source = named_prompt_sections_with_style(&ctx, None)
            .iter()
            .map(PromptSection::text_prompt)
            .collect();
        let system = lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            source,
            None,
            None,
        );
        let blocks = lingxi_llm_client::providers::anthropic::system_prompt::project_system_prompt(
            &system,
            None,
            "claude-sonnet-4-5",
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            Default::default(),
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block.text, s);
        assert!(blocks[0].block.text.starts_with(HEADER));
    }
}
