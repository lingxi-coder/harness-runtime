//! Production prompt byte locks and source-vector projection checks.
//!
//! Digests track LingXi's normalized output. Fixed-artifact native fixtures and
//! source oracles independently establish upstream parity. Each rebase records
//! its cause in the provenance block below.

use std::path::{Path, PathBuf};

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use orchestrator::prompt::{
    assemble_system_prompt, assemble_system_prompt_with_style, env_meta, ActiveOutputStyle,
    FileTree, SystemPromptContext,
};
use sha2::{Digest, Sha256};
use tool_api::tool_trait::{
    ToolInputRepairMetadata, ToolInputRepairSchema, ToolInputSchemaProvenance,
};
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
        native_system_turns: None,
        prompt_policy: None,
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
            } else if line.starts_with(" - OS Version:") {
                " - OS Version: <OS_VERSION>".to_string()
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

#[test]
fn live_prompt_normalization_removes_host_os_version() {
    for os_version in [
        "Darwin 24.6.0",
        "Linux 6.14.0-1017-azure",
        "Windows_NT 10.0",
    ] {
        let prompt = format!("before\n - OS Version: {os_version}\nafter");
        assert_eq!(
            normalize_live_prompt(&prompt, Path::new("/workspace")),
            "before\n - OS Version: <OS_VERSION>\nafter"
        );
    }
}

struct PromptAgentTool {
    schema: Utf16JsonProjection,
}

impl PromptAgentTool {
    fn new() -> Self {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        });
        Self {
            schema: Utf16JsonProjection::from_serde_value(&schema)
                .expect("generated prompt tool schema is valid"),
        }
    }
}

#[async_trait::async_trait]
impl tool_api::Tool for PromptAgentTool {
    fn name(&self) -> &str {
        "Agent"
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        &self.schema
    }

    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn is_enabled(&self, _ctx: &tool_api::ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1_024
    }

    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        input: &Utf16JsonProjection,
        _ctx: &tool_api::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "parity fixture".into(),
            },
            updated_input: Some(input.clone()),
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(
        &self,
        _input: &Utf16JsonProjection,
        _opts: &tool_api::DescriptionOptions,
    ) -> String {
        "Deterministic Agent fixture".to_string()
    }

    async fn prompt(&self, _opts: &tool_api::PromptOptions) -> Utf16JsonProjection {
        "Deterministic Agent fixture".into()
    }

    async fn call(
        &self,
        _input: Utf16JsonProjection,
        _ctx: tool_api::ToolUseContext,
        _progress_tx: tool_api::ToolProgressSender,
    ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        Ok(tool_api::ToolCallResult::from_data(
            Utf16JsonProjection::from_serde_value(&serde_json::json!({ "ok": true }))
                .expect("generated tool fixture is valid"),
        ))
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
/// 2.1.319 synchronization (2026-11-09): all model and style locks move to
/// the pinned artifact 1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd.
/// Main-body and memory protocols changed; environment and model identity now
/// travel in typed request ingress. Classic models can therefore share body
/// hashes while their request identities remain distinct. Upstream 2.1.319 also
/// retains proactive guidance with explicit output styles. These own-output
/// locks were measured through production after the six complete native-body
/// fixtures and source oracles passed; they do not replace that independent
/// upstream evidence. The live test checks ingress on the actual mock request.
///
/// Claude emits one extra first block carrying a private
/// `x-anthropic-billing-header` attestation. LingXi deliberately does not
/// spoof that private billing identity. This fixture uses no provider route
/// and carries the visible prompt as one source element; its body locks exclude
/// the permitted brand header. Actual cache/source grouping is checked separately.
#[test]
fn production_prompt_bodies_match_their_byte_locks() {
    let temp = tempfile::tempdir().expect("temp cwd");
    let cwd = temp.path().canonicalize().expect("physical cwd");
    let memory_dir = cwd.join(".lingxi/projects/oracle/memory");
    let cases = [
        (
            "claude-fable-5-1",
            11_673,
            "ceebcf08808be0cb794b1ee2c1432edc95e70c78a55120ac037e0e838ef61bf0",
        ),
        (
            "claude-haiku-4-5-20251001",
            26_991,
            "f411342e4bb96a9b94bb6f18ae45618e662497b0cf5ae806e75f3a8ff78c8b85",
        ),
        (
            "claude-mythos-5-1",
            10_986,
            "caa7171ee4f05683f6d25cb83fd58c0890162cb801c2d81b7f3b1ab544784280",
        ),
        (
            "claude-opus-4-5",
            26_991,
            "f411342e4bb96a9b94bb6f18ae45618e662497b0cf5ae806e75f3a8ff78c8b85",
        ),
        (
            "claude-opus-4-6",
            26_991,
            "f411342e4bb96a9b94bb6f18ae45618e662497b0cf5ae806e75f3a8ff78c8b85",
        ),
        (
            "claude-opus-4-7",
            26_991,
            "f411342e4bb96a9b94bb6f18ae45618e662497b0cf5ae806e75f3a8ff78c8b85",
        ),
        (
            "claude-opus-4-8",
            5_425,
            "835d85be520ccff6c38cd94fb8f5cab43443e8a74653abb0b844684f117a8496",
        ),
        (
            "claude-opus-5",
            8_875,
            "19007d31ee05647ebefb1cdeb62c1bf8d78c11e7944a8708a1b610bd737979c6",
        ),
        (
            "claude-sonnet-4-6",
            26_991,
            "f411342e4bb96a9b94bb6f18ae45618e662497b0cf5ae806e75f3a8ff78c8b85",
        ),
        (
            "claude-sonnet-5",
            26_991,
            "f411342e4bb96a9b94bb6f18ae45618e662497b0cf5ae806e75f3a8ff78c8b85",
        ),
    ];

    for (model, expected_len, expected_sha) in cases {
        let assembled =
            assemble_system_prompt(&prompt_context(model, cwd.clone(), memory_dir.clone()));
        // This visible-body fixture carries one source element. Provider/cache
        // section boundaries are covered by the SDK's source-vector tests.
        let system = lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![assembled.clone().into()],
            None,
            None,
        );
        let blocks = lingxi_llm_client::providers::anthropic::system_prompt::project_system_prompt(
            &system,
            None,
            model,
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            Default::default(),
        );
        assert_eq!(
            blocks.len(),
            1,
            "{model}: no provider metadata in this fixture"
        );
        let expected_units = assembled.encode_utf16().collect::<Vec<_>>();
        assert_eq!(
            blocks[0].block.text_payload().root_string_code_units(),
            Some(expected_units)
        );
        let body_text = assembled
            .strip_prefix("You are LingXi, an agentic command-line coding assistant.\n\n")
            .expect("brand header followed by the complete two-LF section separator");
        assert!(
            body_text.starts_with('\n'),
            "{model}: body retains leading LF"
        );

        let body = normalize_prompt_body(body_text, &cwd, &memory_dir);
        let digest = format!("{:x}", Sha256::digest(body.as_bytes()));
        assert_eq!(body.len(), expected_len, "{model}: body length drifted");
        assert_eq!(digest, expected_sha, "{model}: normalized body drifted");
    }
}

/// Exercise the live builder and verify typed context on the real request.
/// The body lock was rebased with the 2.1.319 synchronization documented above;
/// environment, model identity and project instructions are checked separately
/// in the captured request rather than assumed to be system-body suffixes.
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
    let api = Arc::new(MockApiClient::new(vec![
        orchestrator::test_support::mock_message_response(
            vec![serde_json::from_value(serde_json::json!({
                "type": "text", "text": "Done."
            }))
            .unwrap()],
            Some("end_turn"),
        ),
    ]));
    let orchestrator = ConversationOrchestrator::new(
        config,
        api.clone(),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![MemoryFile {
            parent: None,
            source_content: None,
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

    let prompt = orchestrator
        .assemble_system_prompt_preview()
        .await
        .expect("production system prompt preview must succeed");
    assert!(
        !prompt.contains("Primary working directory:"),
        "the preview body leaves environment facts in typed ingress"
    );
    assert!(
        prompt.contains("If you need the user to run a shell command themselves"),
        "interactive production prompt must include the ! command guidance"
    );
    assert!(
        !prompt.contains("The exact model ID is"),
        "model identity is rendered by typed request ingress"
    );
    assert!(
        prompt.contains("Calling Agent with subagent_type: \"fork\" creates a fork"),
        "live tool registry must drive the production session guidance"
    );
    let additional_context = orchestrator
        .additional_context_preview()
        .await
        .unwrap()
        .expect("live memory must produce additional context");
    assert!(
        additional_context.contains(memory_body),
        "live memory provider must contribute through the production additional-context path"
    );

    orchestrator
        .run_turn("Verify the supplied context.")
        .await
        .unwrap();
    let requests = api.captured_msgs().await;
    assert_eq!(requests.len(), 1);
    let request_context = serde_json::to_string(&requests[0]).unwrap();
    assert!(request_context.contains(&format!("Primary working directory: {}", cwd.display())));
    assert!(request_context.contains("The exact model ID is claude-sonnet-5."));
    assert!(request_context.contains(memory_body));

    let normalized = normalize_live_prompt(&prompt, &cwd);
    let digest = format!("{:x}", Sha256::digest(normalized.as_bytes()));
    assert_eq!(normalized.len(), 14_274, "live prompt length drifted");
    assert_eq!(
        digest, "c65f40d524d232641e107e8398d6a447450c14d7955507adad85acf5690278b1",
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
            10_283,
            "0ed9e8cef363da7794f59dd2971ad8e0fe14b73a75dde042d5892f66d2099787",
        ),
        (
            "Concise",
            10_300,
            "7f0dfa4fd73f2428c329aed423bfeedbad43007354bcb403ec660b933a1a3f51",
        ),
        (
            "Explanatory",
            10_140,
            "3c8af9089f234fd191378718c5661d719a223aad35359203ea89021e2c16b3ef",
        ),
        (
            "Learning",
            14_016,
            "ad638f6be84002cabf40c7b00984418207c568ca661af5d3c7654f7fe96018da",
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
        // This visible-body fixture carries one source element. Provider/cache
        // section boundaries are covered by the SDK's source-vector tests.
        let system = lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![assembled.clone().into()],
            None,
            None,
        );
        let blocks = lingxi_llm_client::providers::anthropic::system_prompt::project_system_prompt(
            &system,
            None,
            "claude-opus-5",
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            Default::default(),
        );
        assert_eq!(blocks.len(), 1);
        let expected_units = assembled.encode_utf16().collect::<Vec<_>>();
        assert_eq!(
            blocks[0].block.text_payload().root_string_code_units(),
            Some(expected_units)
        );
        let body_text = assembled
            .strip_prefix("You are LingXi, an agentic command-line coding assistant.\n\n")
            .expect("brand header followed by the complete two-LF section separator");

        let body = normalize_prompt_body(body_text, &cwd, &memory_dir);
        let digest = format!("{:x}", Sha256::digest(body.as_bytes()));
        assert_eq!(
            body.len(),
            expected_len,
            "{style_name}: body length drifted"
        );
        assert_eq!(
            digest, expected_sha,
            "{style_name}: normalized body drifted"
        );
        assert!(
            body.contains("When you have enough information to act"),
            "{style_name}: 2.1.319 retains proactive guidance with an explicit output style"
        );
    }
}

#[test]
fn non_claude_models_use_generic_lean_without_claude_metadata_leakage() {
    let context = prompt_context(
        "vendor-compat-claude-sonnet-5",
        PathBuf::from("/not-materialized"),
        PathBuf::from("/memory"),
    );
    let prompt = assemble_system_prompt(&context);

    assert!(prompt.contains("# Working with the user"));
    assert!(prompt.contains("# Tools and actions"));
    assert!(prompt.contains("# Memory"));
    assert!(!prompt.contains("# auto memory"));
    assert!(!prompt.contains("# Communicating with the user"));
    assert!(!prompt.contains("The most recent Claude models are"));
    assert_eq!(
        lingxi_core::host::prompt_policy::resolve_prompt_policy(
            lingxi_core::host::prompt_policy::PromptTarget::for_model(
                lingxi_core::host::session_context::ConversationMode::Code,
                &context.model,
            )
        )
        .bundle_id,
        "generic-code-lean-v1"
    );
}
