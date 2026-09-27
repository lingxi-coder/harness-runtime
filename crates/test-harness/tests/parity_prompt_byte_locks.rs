//! Byte locks on the prompt surface this port assembles.
//!
//! Deliberately NOT named after a version. Every digest below is a hash of
//! LingXi's OWN normalized output, so it catches unintended drift and says
//! nothing about which oracle release the text matches — the oracle-derived
//! truth lives in the per-sentence assertions in
//! `orchestrator/src/prompt/body_sections.rs` and `env_block.rs`. The values
//! have been re-based four times now (see the PROVENANCE block below); a
//! version in the file name would have gone stale at the first of them, and
//! did: these tests spent three re-bases inside `parity_claude_2_1_220.rs`
//! asserting 2.1.267 content.

use std::path::{Path, PathBuf};

use orchestrator::prompt::{
    assemble_system_prompt, assemble_system_prompt_with_style, env_meta, ActiveOutputStyle,
    FileTree, SystemPromptContext,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
fn prompt_tool_names() -> Vec<String> {
    [
        "Agent",
        "Bash",
        "CronCreate",
        "CronDelete",
        "CronList",
        "Edit",
        "EnterWorktree",
        "ExitWorktree",
        "NotebookEdit",
        "Read",
        "ReportFindings",
        "ScheduleWakeup",
        "SendMessage",
        "Skill",
        "TaskCreate",
        "TaskGet",
        "TaskList",
        "TaskOutput",
        "TaskStop",
        "TaskUpdate",
        "WebFetch",
        "WebSearch",
        "Workflow",
        "Write",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn prompt_context(model: &str, cwd: PathBuf, memory_dir: PathBuf) -> SystemPromptContext {
    SystemPromptContext {
        cwd,
        platform: "darwin".to_string(),
        model: model.to_string(),
        model_marketing_name: env_meta::marketing_name_for_model(model).map(str::to_string),
        knowledge_cutoff: env_meta::knowledge_cutoff_for_model(model).map(str::to_string),
        shell: "zsh".to_string(),
        os_version: "Darwin 24.6.0".to_string(),
        git_status: None,
        in_worktree: false,
        file_tree: FileTree::default(),
        memory_files: Vec::new(),
        tool_names: prompt_tool_names(),
        skills_available: true,
        is_interactive: false,
        memory_dir: Some(memory_dir),
        exclude_dynamic_sections: false,
    }
}

fn normalize_prompt_body(body: &str, cwd: &Path, memory_dir: &Path) -> String {
    // Replace the nested memory path first; replacing cwd first would turn it
    // into `<CWD>/…` and leave the manifest dependent on the test layout.
    body.replace(&memory_dir.to_string_lossy().to_string(), "<MEMORY_DIR>")
        .replace(&cwd.to_string_lossy().to_string(), "<CWD>")
}

fn normalize_live_prompt(body: &str, cwd: &Path) -> String {
    body.replace(&cwd.to_string_lossy().to_string(), "<CWD>")
        .split('\n')
        .map(|line| {
            if line.starts_with(" - Primary working directory:") {
                " - Primary working directory: <CWD>".to_string()
            } else if line.starts_with(" - Platform:") {
                " - Platform: <PLATFORM>".to_string()
            } else if line.starts_with(" - Shell:") {
                " - Shell: <SHELL>".to_string()
            } else if line.starts_with(" - OS version:") {
                " - OS version: <OS_VERSION>".to_string()
            } else if line.starts_with("Today's date is ")
                || line.starts_with("The current date is ")
            {
                "<CURRENT_DATE>".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct PromptAgentTool {
    schema: Value,
}

impl PromptAgentTool {
    fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }
}

#[async_trait::async_trait]
impl tool_api::Tool for PromptAgentTool {
    fn name(&self) -> &str {
        "Agent"
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    fn is_enabled(&self, _ctx: &tool_api::ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1_024
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        input: &Value,
        _ctx: &tool_api::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "parity fixture".to_string(),
            },
            updated_input: Some(input.clone()),
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(&self, _input: &Value, _opts: &tool_api::DescriptionOptions) -> String {
        "Deterministic Agent fixture".to_string()
    }

    async fn prompt(&self, _opts: &tool_api::PromptOptions) -> String {
        "Deterministic Agent fixture".to_string()
    }

    async fn call(
        &self,
        _input: Value,
        _ctx: tool_api::ToolUseContext,
        _progress_tx: tool_api::ToolProgressSender,
    ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        Ok(tool_api::ToolCallResult {
            data: serde_json::json!({"ok": true}),
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// Byte manifest for the prompt assembler — **regenerated for claude-code
/// 2.1.238 on 2026-08-20**.
///
/// PROVENANCE, so nobody mistakes these for oracle facts: these lengths and
/// digests are hashes of LingXi's OWN normalized prompt body. They are a
/// regression lock that catches unintended prompt drift; they are NOT extracted
/// from the Claude Code binary. The oracle-derived truth for this prompt lives
/// in the per-sentence assertions in `orchestrator/src/prompt/body_sections.rs`
/// and `env_block.rs`, which is where a byte claim must actually be proven.
///
/// They moved from the 2.1.220 values because the 2.1.238 alignment pass
/// rewrote `# Communicating with the user` (SP-1…SP-4: four em-dash clauses
/// re-punctuated, "the PR merges" -> "the change merges"), the `action_caution`
/// tail (SP-5), the Fable/Mythos autonomy tail (SP-6), the fork bullet (SP-7),
/// and dropped Opus 4.7 from the `# Environment` fast-mode line (SP-8).
///
/// SECOND MOVE, 2026-09-02 — the values below are NOT the ones the 2.1.238
/// pass produced. `main` commit 5689bf092 ("Keep provider choices aligned with
/// official model catalogs") changed the assembled bytes and did not touch this
/// manifest, so these three byte locks —
/// `production_prompt_bodies_match_their_byte_locks`,
/// `live_orchestrator_prompt_uses_production_context` and
/// `production_output_style_bodies_match_their_byte_locks` — were
/// regenerated against that commit's output. Exactly two things moved:
///
/// 1. `env_block.rs` renders the `# Environment` model-catalog line as
///    `Model IDs — Fable 5.1: 'claude-fable-5-1', …` where it used to render
///    `Model IDs — Fable 5: 'claude-fable-5', …`. That is +4 bytes ("5" ->
///    "5.1" and `claude-fable-5` -> `claude-fable-5-1`), and it is why EVERY
///    case here except the two renamed ones, BOTH output styles, and the live
///    prompt each grew by exactly 4 bytes and got a new digest.
/// 2. The Fable/Mythos profile ids themselves were renamed
///    (`claude-fable-5` -> `claude-fable-5-1`, `claude-mythos-5` ->
///    `claude-mythos-5-1`) in `body_sections::is_communicating_model` /
///    `post_context_sections`, and `FABLE_IDENTITY_SECTION` was rewritten.
///    Those two cases therefore changed wholesale (10_449 -> 10_204 and
///    9_755 -> 10_206), not by 4 bytes.
///
/// THIRD MOVE, 2026-09-07 — Fable identity (`nss`) aligned to Claude Code
/// 2.1.263. `FABLE_IDENTITY_SECTION` grew 241 bytes (Glasswing / platform-docs
/// rewrite replaced by the binary paragraph + anthropic.com/claude/fable).
/// Only `claude-fable-5-1` (10_204 -> 10_445) and `claude-mythos-5-1`
/// (10_206 -> 10_447) moved.
///
/// FOURTH MOVE, 2026-09-10 — two separate `main` alignments, neither of which
/// re-blessed these numbers at the time. They are recorded together because
/// they were found together, not because they are related.
///
/// 1. −9 on the **`claude-opus-5` profile only**: `3537e08cf` (2.1.263) rewrote
///    `body_sections::OPUS_5_TERMINAL_RESTRICTIONS` from the two-line
///    `"Do not call the AgentTool …\nDo not use workflows or deep-research …"`
///    (121 bytes) to the single-line `"Do not use the Agent tool, workflows, or
///    deep-research unless the user, a LINGXI.md file, or a skill asks for it"`
///    (112 bytes). This reaches three cases, and all three for the same reason:
///    `claude-opus-5` itself, and BOTH output styles, which
///    `production_output_style_bodies_match_their_byte_locks`
///    renders with `prompt_context("claude-opus-5", …)`.
///
///    ⚠️ This one had been stale since `b09e21789`, the commit that recorded
///    the THIRD MOVE above: building that commit's own tree produces 9_304 for
///    `claude-opus-5` against the 9_313 it wrote down. The Fable/Mythos halves
///    of that re-bless were correct; `claude-opus-5` was simply not re-measured.
///    Measured, not inferred — the tree was extracted and built.
///
/// 2. −3 on **every** case: `972ca7f33` (2.1.267) re-based the memory
///    frontmatter separator, `description: <one-line summary — used to decide
///    relevance…>` to `<one-line summary, used to decide relevance…>`. The
///    em-dash is U+2014, three bytes, and each assembled body renders the line
///    exactly once (counted, per model, on the normalized body).
///
/// So `claude-opus-5`, `Explanatory` and `Learning` moved −12 and every other
/// case moved −3. `live_orchestrator_prompt_uses_production_context` did NOT
/// move: its normalized prompt contains neither changed string, and its 14_602
/// byte lock is unchanged.
///
/// Nothing outside `main` contributed: `orchestrator/src/prompt/` is
/// byte-identical to `main` on this branch (`git diff main HEAD --` is empty
/// for that directory), so every byte above is `main`'s own output. When a
/// future re-bless is needed, extend this note the same way — a bare number
/// change with no named cause is indistinguishable from the unintended drift
/// these locks exist to catch.
///
/// Claude emits one extra first block carrying a private
/// `x-anthropic-billing-header` attestation. LingXi deliberately does not
/// spoof that private billing identity, so its request contains the remaining
/// two observable blocks: the LingXi brand prefix and the normalized body.
#[test]
fn production_prompt_bodies_match_their_byte_locks() {
    let temp = tempfile::tempdir().expect("temp cwd");
    let cwd = temp.path().canonicalize().expect("physical cwd");
    let memory_dir = cwd.join(".lingxi/projects/oracle/memory");
    let cases = [
        (
            "claude-fable-5-1",
            10_442,
            "321a0dc5abedf4c99a2a63a39f8a950c87ed21f66ed62bc93098dfe6a9bca5bc",
        ),
        (
            "claude-haiku-4-5-20251001",
            27_531,
            "ca95b52c17e71163fe111875d5ce17693faf00c186775f3f09ad1ae2240d7c09",
        ),
        (
            "claude-mythos-5-1",
            10_444,
            "59edba1d7761f0ff71eb0f629664d9e2d968beb30134e72f2c0aeda4a8572951",
        ),
        (
            "claude-opus-4-5",
            27_515,
            "9d2bd6aa81cccd4678f9466acab5919bb41bb1e6193586b2115d9ba3c2d95148",
        ),
        (
            "claude-opus-4-6",
            27_515,
            "7e171ab72e680671dea567ccf1d0ccac8bf9819cc43e7c14a59efe04914ce15f",
        ),
        (
            "claude-opus-4-7",
            27_519,
            "50845105872aff4f4b83ef915563282748f1f61e4195f044c69197a0b107825a",
        ),
        (
            "claude-opus-4-8",
            5_971,
            "5970d3bf23bf4e9a4363900e51ca5303ada519fd961441095992955d0465e0e1",
        ),
        (
            "claude-opus-5",
            9_301,
            "b89957b3af9694a5bb371527e60dd963bf69913fe37456bb97afcff13a8c7438",
        ),
        (
            "claude-sonnet-4-6",
            27_522,
            "c7c420b349e953a0ae832f675e98bf022300d2227f4e4a187de71f265da3dfeb",
        ),
        (
            "claude-sonnet-5",
            27_519,
            "3555fdf60684429183e0c2cd372f194e63db4e4cae28b24e95878a157b835a5d",
        ),
    ];

    for (model, expected_len, expected_sha) in cases {
        let assembled =
            assemble_system_prompt(&prompt_context(model, cwd.clone(), memory_dir.clone()));
        let blocks = llm_runtime::prompt_format::split_system_blocks(&assembled, true);
        assert_eq!(
            blocks.len(),
            2,
            "{model}: private Anthropic billing attestation must not be fabricated"
        );
        assert_eq!(
            blocks[0].text,
            "You are LingXi, an agentic command-line coding assistant."
        );
        assert!(
            blocks[1].text.starts_with('\n'),
            "{model}: the body block retains Claude's leading LF"
        );

        let body = normalize_prompt_body(&blocks[1].text, &cwd, &memory_dir);
        assert_eq!(body.len(), expected_len, "{model}: body length drifted");
        let digest = format!("{:x}", Sha256::digest(body.as_bytes()));
        assert_eq!(digest, expected_sha, "{model}: normalized body drifted");
    }
}

/// Exercise the same live prompt builder used by real turns. The byte manifests
/// above intentionally use a synthetic context so dynamic machine fields can be
/// normalized deterministically; this test prevents that stable fixture from
/// masking a broken composition path.
///
/// Its 14_602/`06da73ad…` byte lock shares the PROVENANCE block above
/// `production_prompt_bodies_match_their_byte_locks` — read it
/// before changing either value, and record WHY the number moved.
#[tokio::test]
async fn live_orchestrator_prompt_uses_production_context() {
    use std::sync::Arc;
    use std::sync::{Mutex, OnceLock};

    use memory::lingxi_md::LingxiMdTier;
    use orchestrator::prompt::MemoryFile;
    use orchestrator::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
    use tool_api::registry::ToolRegistry;

    static PROMPT_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _prompt_env_guard = PROMPT_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let prompt_env_keys = [
        "LINGXI_FORK_SUBAGENT",
        "LINGXI_SESSION_KIND",
        "LINGXI_JOB_DIR",
        "LINGXI_BG_ISOLATION",
        "LINGXI_ACT_DONT_REDERIVE",
        "CLAUDE_CODE_ACT_DONT_REDERIVE",
    ];
    let saved_prompt_env = prompt_env_keys.map(|key| (key, std::env::var_os(key)));
    for key in prompt_env_keys {
        std::env::remove_var(key);
    }
    struct RestorePromptEnv([(&'static str, Option<std::ffi::OsString>); 6]);
    impl Drop for RestorePromptEnv {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
    let _restore_prompt_env = RestorePromptEnv(saved_prompt_env);

    let temp = tempfile::tempdir().expect("temp cwd");
    let cwd = temp.path().canonicalize().expect("physical cwd");
    let memory_body = "Use deterministic production-path memory.";
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(PromptAgentTool::new()));
    let config = OrchestratorConfig {
        model: "claude-sonnet-5".to_string(),
        interactive_permissions: false,
        interactive_session: true,
        ..OrchestratorConfig::default()
    };
    let orchestrator = ConversationOrchestrator::new(
        config,
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![MemoryFile {
            path: cwd.join("LINGXI.md"),
            body: memory_body.to_string(),
            is_local_override: false,
            tier: LingxiMdTier::Project,
            globs: None,
            raw_content: memory_body.to_string(),
            content_differs_from_disk: false,
        }])),
        cwd.clone(),
    );

    let prompt = orchestrator.assemble_system_prompt_preview().await;
    assert!(
        prompt.contains(&format!(" - Primary working directory: {}", cwd.display())),
        "live cwd must come from the production prompt context"
    );
    assert!(
        prompt.contains("If you need the user to run a shell command themselves"),
        "interactive production prompt must include the ! command guidance"
    );
    assert!(
        prompt.contains("The exact model ID is claude-sonnet-5."),
        "live model must drive the production environment block"
    );
    assert!(
        prompt.contains("Calling Agent with subagent_type: \"fork\" creates a fork"),
        "live tool registry must drive the production session guidance"
    );
    let additional_context = orchestrator
        .additional_context_preview()
        .await
        .expect("live memory must produce additional context");
    assert!(
        additional_context.contains(memory_body),
        "live memory provider must contribute through the production additional-context path"
    );

    let normalized = normalize_live_prompt(&prompt, &cwd);
    let digest = format!("{:x}", Sha256::digest(normalized.as_bytes()));
    assert_eq!(normalized.len(), 14_602, "live prompt length drifted");
    assert_eq!(
        digest, "06da73ad932d83116628c510ea628befd77b0f87b6f1fbe2df041c30e73bc54c",
        "live normalized production prompt drifted"
    );
}

/// Byte lock for the four builtin output-style bodies. Its lengths and
/// digests share the PROVENANCE block above
/// `production_prompt_bodies_match_their_byte_locks` — read it
/// before changing a number here, and record WHY the number moved.
#[test]
fn production_output_style_bodies_match_their_byte_locks() {
    let temp = tempfile::tempdir().expect("temp cwd");
    let cwd = temp.path().canonicalize().expect("physical cwd");
    let memory_dir = cwd.join(".lingxi/projects/oracle/memory");
    let cases = [
        // cc2.1.270 built-ins this port had never shipped.
        (
            "Proactive",
            10_455,
            "42f817633fe318b5411275733e670555b0508c3894b4d5c3742b14a8b37fc460",
        ),
        (
            "Concise",
            10_472,
            "ae01b874c2384a31c722f5661db14c4eac3d26ed2cd219af6677c55051edd7eb",
        ),
        (
            "Explanatory",
            10_312,
            "3064865b24b6bf0f1e93efc45997b4ae5193b59828e40ea0c79bbc0445879b8c",
        ),
        (
            "Learning",
            14_188,
            "3b8cf74ef28ee660139815cee4aab374fe71af5549b9e932b6a6b39a17a48913",
        ),
    ];

    for (style_name, expected_len, expected_sha) in cases {
        let style = outputstyles::resolve_builtin_output_style(Some(style_name))
            .expect("builtin output style");
        let assembled = assemble_system_prompt_with_style(
            &prompt_context("claude-opus-5", cwd.clone(), memory_dir.clone()),
            Some(ActiveOutputStyle {
                name: style.name,
                prompt: style.prompt,
                keep_coding_instructions: style.keep_coding_instructions,
            }),
        );
        let blocks = llm_runtime::prompt_format::split_system_blocks(&assembled, true);
        assert_eq!(blocks.len(), 2);

        let body = normalize_prompt_body(&blocks[1].text, &cwd, &memory_dir);
        assert_eq!(
            body.len(),
            expected_len,
            "{style_name}: body length drifted"
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(body.as_bytes())),
            expected_sha,
            "{style_name}: normalized body drifted"
        );
        assert!(
            !body.contains("When you have enough information to act"),
            "{style_name}: explicit style must suppress act-don't-rederive"
        );
    }
}

#[test]
fn non_claude_models_keep_full_harness_without_claude_metadata_leakage() {
    let context = prompt_context(
        "vendor-compat-claude-sonnet-5",
        PathBuf::from("/not-materialized"),
        PathBuf::from("/memory"),
    );
    let prompt = assemble_system_prompt(&context);

    assert!(prompt.contains("# System"));
    assert!(prompt.contains("# Text output (does not apply to tool calls)"));
    assert!(prompt.contains("# auto memory"));
    assert!(!prompt.contains("# Communicating with the user"));
    assert!(!prompt.contains("The most recent Claude models are"));
    assert_eq!(
        platform_api::model_capabilities::prompt_profile_for(&context.model),
        platform_api::model_capabilities::PromptProfile::FullHarness
    );
}
