//! F1-08 — JSON-schema golden snapshots (the frozen wire contract).
//!
//! This is the single most important F1 deliverable: it serializes ONE canonical
//! instance of EVERY `ClientEvent` / `ClientCommand` variant + the `MessageDto`
//! block set + each permission / error DTO into checked-in
//! `client::protocol/snapshots/*.json` goldens, plus a `feed_status.json` golden
//! enumerating the `RenderedMessage` feed-status table (LIVE-FED vs.
//! RESERVED/feed-deferred). The snapshot IS the frozen wire format and the
//! auditable feed-status record (plan F1-08, governing decisions §0.7 / §0.9).
//!
//! ## Framework choice (plan F1-08)
//!
//! The plan says: "`insta` if already a workspace dev-dep, else a hand-rolled
//! `assert_eq!(serde_json::to_string_pretty(&x), include_str!(golden))` (no new
//! prod dep — dev-only)." `insta` is NOT a `[workspace.dependencies]` entry — it
//! is declared per-crate in `tui` / `engine-*` with a literal version, never as
//! a shared workspace dev-dep — so this harness is the sanctioned hand-rolled
//! variant: `serde_json::to_string_pretty` compared against a checked-in golden
//! read from disk. `serde_json` is a DEV-ONLY dep (it must never enter the
//! contract crate itself, §0.4).
//!
//! ## Regenerating goldens
//!
//! Run with `BLESS=1` to (re)write every golden from the current canonical
//! instances, then review the diff before committing:
//!
//! ```text
//! BLESS=1 cargo test -p client --test snapshot_test
//! ```
//!
//! The "red" at F1-08 is that NO goldens exist yet, so every case fails with a
//! missing-file error; `BLESS=1` generates them, the goldens are reviewed, and a
//! plain run goes green. Any later structural drift (a renamed tag, a retyped
//! field, a dropped variant) flips the matching golden and the test fails — that
//! is the contract-freeze guarantee.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use client::protocol::ask_user_question::{
    AskOptionDto, AskQuestionDto, AskUserQuestionRequestDto,
};
use client::protocol::audio::{
    AudioCapabilitySnapshotDto, AudioOperationDto, AudioOperationIdDto, AudioOperationKindDto,
    AudioOperationReadinessDto, AudioOperationRequestDto, AudioOperationResultDto, AudioOwnerDto,
    AudioReadinessStateDto,
};
use client::protocol::commands::{
    AppCreateModeDto, ClientCommand, HookAdminCommandDto, ImageRefDto, ListingKindDto,
    McpAdminCommandDto, PermissionBehaviorDto, PluginAdminCommandDto, PromptModeDto,
    ProviderCredentialSecretDto, SkillAdminCommandDto, UiSurfaceDto, WritableScopeDto,
};
use client::protocol::computer_access::{
    AccessTierDto, ComputerAccessRequestDto, ComputerAccessResponseDto, RequestedAppDto,
    TccStateDto,
};
use client::protocol::controls::{
    ControlDisabledReasonDto, ConversationControlsDto, PermissionControlStateDto,
    PermissionModeOptionDto, ReasoningBudgetRangeDto, ReasoningControlSpecDto,
    ReasoningControlStateDto, ReasoningOptionDto, ReasoningSelectionDto,
};
use client::protocol::error::ClientError;
use client::protocol::events::{
    AttachmentDto, ClientEvent, CostDto, ErrorKindDto, RefusalContinuationJoinDto,
    RefusalContinuationPhaseDto, ServerFallbackProviderMessageDto,
    ServerFallbackTombstoneMessageDto, TurnOutcomeDto, TurnRecoverySnapshotDto,
    TurnRecoveryStateDto,
};
use client::protocol::listings::{
    AgentDto, AuthStateDto, CheckStatusDto, ConfigurationDomainDto, ConfigurationEffectDto,
    ConfigurationOperationStatusDto, CoordinatorWorkerDto, DoctorCheckDto, DoctorReportDto,
    DoctorSummaryDto, HookDto, McpServerDto, McpStatusDto, MemoryEntryDto, MemoryTierDto,
    SessionAgentMessageRowDto, SessionAgentSummaryDto, SessionModeDto, SessionRowDto, SkillDto,
    SlashCommandDto, StatusSnapshotDto, TaskRowDto, TaskStatusDto,
};
use client::protocol::local_apps::{
    AppAgentProfileProposalDto, AppAuthorizationDecisionDto, AppBridgeOperationDto,
    AppBridgeRequestDto, AppBridgeResponseDto, AppCapabilityKindDto, AppCapabilityRequestDto,
    AppCheckpointDto, AppCheckpointKindDto, AppCreateOriginDto, AppDataCollectionDto,
    AppDataFieldDto, AppDataFieldTypeDto, AppDependencyChangeConfirmationRequestDto,
    AppDependencyChangeDto, AppDependencyChangeKindDto, AppDependencySnapshotDto, AppDetailsDto,
    AppErrorCodeDto, AppEventDto, AppManifestDto, AppRecordDto, AppRuntimeDetailsDto,
    AppRuntimeModeDto, AppRuntimeProfileBindingDto, AppRuntimeProfileDto,
    AppRuntimeProfileOptionDto, AppRuntimeProfilePackageDto, AppRuntimeRecoveryStateDto,
    AppRuntimeStateDto, AppRuntimeSuspensionReasonDto, AppSessionKindDto, AppSessionRowDto,
    AppSurfaceDto, AppUiActionKindDto, AppUiRequestDto, AppWorkflowStateDto, DeviceContextDto,
    LocalAppCreateConfirmationRequestDto, LocalAppGateStatusDto,
    LocalAppMcpProposalApprovalRequestDto, LocalAppMcpToolChangeKindDto, LocalAppMcpToolDiffDto,
    LocalAppMcpToolFieldDto, LocalAppMcpToolSurfaceDto, LocalAppPluginComponentCountsDto,
    LocalAppPluginErrorCodeDto, LocalAppPluginInventoryDto, LocalAppRejectedCandidateDto,
    LocalAppTemplateSummaryDto, LocalAppVerificationStatusDto, LocalAppVerificationSummaryDto,
    ManagedLocalAppMcpServerDto, ManagedLocalAppMcpStatusDto, McpAppWidgetDto,
    PluginActivationStateDto, PluginCommandDto, PluginStatusDto,
};
use client::protocol::message::{MessageBlockDto, MessageDto};
use client::protocol::permission::{
    PermissionKindDto, PermissionRequest, PermissionResolutionDto, PermissionResolved,
    PermissionResponseDto, WorkerInfoDto,
};
use client::protocol::tool_display::{
    CodeSegmentDto, DiffLineKindDto, DiffRowDto, HeadlineKindDto, PlanTaskDto, PlanTaskStateDto,
    StructuredDiffDto, SyntaxClassDto, ToolHeaderDto, ToolIconDto, ToolResultDisplayDto,
    ToolVerbDto,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

/// Directory holding the checked-in goldens.
fn snapshots_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("snapshots")
}

/// `true` when the test is invoked in regeneration mode (`BLESS=1`).
fn bless() -> bool {
    matches!(std::env::var("BLESS").as_deref(), Ok("1" | "true"))
}

/// Pretty-print a DTO to the canonical golden string (trailing newline so the
/// file is a well-formed text file and `git diff` is clean).
fn pretty<T: Serialize>(value: &T) -> String {
    let mut s = serde_json::to_string_pretty(value).expect("serialize golden instance");
    s.push('\n');
    s
}

/// Assert one canonical instance matches (or, under `BLESS=1`, (re)writes) its
/// golden. Collects a human-readable failure rather than panicking so a single
/// run reports EVERY drifted golden at once.
fn check_golden<T>(filename: &str, value: &T, failures: &mut Vec<String>)
where
    T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let path = snapshots_dir().join(filename);
    let want = pretty(value);

    if bless() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create snapshots dir");
        }
        fs::write(&path, &want).unwrap_or_else(|e| panic!("write golden {filename}: {e}"));
        return;
    }

    let got = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            failures.push(format!(
                "missing golden `{filename}` ({e}); regenerate with `BLESS=1 cargo test -p client --test snapshot_test`"
            ));
            return;
        }
    };

    if got != want {
        failures.push(format!(
            "golden `{filename}` drifted from the canonical instance — the wire \
             contract changed.\n--- on disk ---\n{got}\n--- canonical ---\n{want}\n\
             If this change is intentional, bump CLIENT_PROTOCOL_VERSION per §0.10 \
             and re-bless with `BLESS=1`."
        ));
    }

    // The golden must also be a faithful, deserializable representation of the
    // value — round-trips through the on-disk JSON byte-stably (decision §0.1).
    let parsed: Value = serde_json::from_str(&got)
        .unwrap_or_else(|e| panic!("golden `{filename}` is not valid JSON: {e}"));
    let back: T = serde_json::from_value(parsed)
        .unwrap_or_else(|e| panic!("golden `{filename}` does not deserialize back: {e}"));
    if &back != value {
        failures.push(format!(
            "golden `{filename}` does not round-trip back to its canonical instance"
        ));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Canonical instances — ONE per variant, with stable field values.
// ─────────────────────────────────────────────────────────────────────────────

/// Every `ClientEvent` variant, paired with its golden filename.
#[allow(clippy::too_many_lines)] // a flat data table: one row per ClientEvent variant
fn event_goldens() -> Vec<(&'static str, ClientEvent)> {
    vec![
        ("event/cron_run_bound.json", ClientEvent::CronRunBound {run_id: "run-1".into(), error: None}),
        ("event/scheduled_run_finished.json", ClientEvent::ScheduledRunFinished {run_id: "run-1".into(), summary: Some("Completed".into()), error: None}),
        ("event/cron_run_requested.json", ClientEvent::CronRunRequested {run_id: "run-1".into(), task: client::protocol::events::CronJobDto {
            next_run_at: None, automation: None, id: "task-1".into(), cron: "0 9 * * *".into(), prompt: "Daily brief".into(), recurring: true, durable: true, permanent: false, created_at: 0, last_fired_at: None, expires_at: None, session_id: None,
        }}),
        (
            "event/message_identity.json",
            ClientEvent::MessageIdentity {
                message_id: "assistant-attempt-1".into(),
            },
        ),
        (
            "event/message_retracted.json",
            ClientEvent::MessageRetracted {
                message_id: "assistant-attempt-1".into(),
            },
        ),
        (
            "event/cron_result.json",
            ClientEvent::CronResult {
                request_id: "cron-1".into(),
                jobs: Vec::new(),
                error: None,
            },
        ),
        (
            "event/error.json",
            ClientEvent::Error {
                kind: ErrorKindDto::Transport,
                message: "connection reset".to_string(),
            },
        ),
        (
            "event/system_notice.json",
            ClientEvent::SystemNotice {
                message: "Conversation changes could not be saved.".to_string(),
                is_error: true,
            },
        ),
        (
            "event/ui_log.json",
            ClientEvent::UiLog {
                plugin: "review".to_string(),
                text: "Found a mismatch".to_string(),
            },
        ),
        (
            "event/ui_toast.json",
            ClientEvent::UiToast {
                plugin: "review".to_string(),
                text: "Found a mismatch".to_string(),
                timeout_ms: 4000,
            },
        ),
        (
            "event/ui_status.json",
            ClientEvent::UiStatus {
                plugin: "review".to_string(),
                text: None,
            },
        ),
        (
            "event/ui_control_result.json",
            ClientEvent::UiControlResult {
                request_id: "render-1".into(),
                response_json: Some(
                    r#"{"tree":{},"props":{},"rewritten":false,"hooked":true}"#.into(),
                ),
                metadata_json: Some(r#"{"renderRevision":7}"#.into()),
                error: None,
            },
        ),
        (
            "event/ui_control_result_error.json",
            ClientEvent::UiControlResult {
                request_id: "module-2".into(),
                response_json: None,
                metadata_json: None,
                error: Some("module unavailable".into()),
            },
        ),
        (
            "event/ui_client_frame.json",
            ClientEvent::UiClientFrame {
                runtime_id: "runtime-1".into(),
                frame_json: r#"{"type":"ui.render","tree":{}}"#.into(),
            },
        ),
        (
            "event/ui_invalidate.json",
            ClientEvent::UiInvalidate {
                instances_json: Some("[]".into()),
                uuid: "event-1".into(),
                session_id: "session-1".into(),
            },
        ),
        (
            "event/task_lifecycle.json",
            ClientEvent::TaskLifecycle {
                event_json: r#"{"type":"system","subtype":"task_started","task_id":"b12345678","description":"background job","task_type":"local_bash"}"#.to_string(),
            },
        ),
        (
            "event/task_list_complete.json",
            ClientEvent::TaskListComplete { request_id: "guard-1".into(), active_count: 2, error: None },
        ),
        (
            "event/scheduled_task_fire.json",
            ClientEvent::ScheduledTaskFire { message: "Scheduled task fired: check the deploy".into() },
        ),
        (
            "event/loop_wakeup.json",
            ClientEvent::LoopWakeup {
                message: "Claude resuming /loop wakeup (Sep 7 3:04pm) \u{b7} 3 no-op ticks since Sep 7 2:14pm".to_string(),
                companion: Some(
                    "[3 prior /loop wakeups found nothing actionable; loop is healthy.]"
                        .to_string(),
                ),
                streak: 3,
                since_ms: 1_788_790_449_000,
            },
        ),
        (
            "event/loop_wakeup_plain.json",
            ClientEvent::LoopWakeup {
                message: "Claude resuming /loop wakeup (Sep 7 3:04pm)".to_string(),
                companion: None,
                streak: 0,
                since_ms: 0,
            },
        ),
        (
            "event/text_delta.json",
            ClientEvent::TextDelta {
                text: "Hello, world.".to_string(),
            },
        ),
        (
            "event/tool_use_started.json",
            ClientEvent::ToolUseStarted {
                id: "toolu_01".to_string(),
                tool: "Read".to_string(),
                input_json: r#"{"file_path":"/tmp/example.txt"}"#.to_string(),
                // Populated, not `None`: every new field is
                // `skip_serializing_if`, so a `None` golden would leave the
                // added wire shape completely unexercised.
                header: Some(ToolHeaderDto {
                    verb: ToolVerbDto::Read,
                    icon: Some(ToolIconDto::Read),
                    label: "Read".to_string(),
                    primary: Some("/tmp/example.txt".to_string()),
                    qualifier: None,
                    count: None,
                    sub_line: None,
                    title: "Read(/tmp/example.txt)".to_string(),
                }),
            },
        ),
        (
            "event/plan_updated.json",
            ClientEvent::PlanUpdated {
                tasks: vec![
                    PlanTaskDto {
                        id: None,
                        subject: "Port the diff renderer".to_string(),
                        active_form: Some("Porting the diff renderer".to_string()),
                        state: PlanTaskStateDto::InProgress,
                    },
                    PlanTaskDto {
                        id: Some("task_2".to_string()),
                        subject: "Regenerate the bindings".to_string(),
                        active_form: None,
                        state: PlanTaskStateDto::Pending,
                    },
                ],
            },
        ),
        (
            "event/workflow_resumed.json",
            ClientEvent::WorkflowResumed {
                previous_task_id: "w12345678".to_string(),
                task: canonical_task_row(),
                run_id: "wf_abcdef".to_string(),
                origin_session_id: Some("session-1".to_string()),
            },
        ),
        (
            "event/tool_heartbeat.json",
            ClientEvent::ToolHeartbeat {
                id: "toolu_01".to_string(),
                tool: "Read".to_string(),
                elapsed_ms: 1_500,
            },
        ),
        (
            "event/tool_use_result.json",
            ClientEvent::ToolUseResult {
                id: "toolu_01".to_string(),
                tool: "Read".to_string(),
                result_json: r#"{"content":"file body"}"#.to_string(),
                is_error: false,
                // Populated so the golden locks the full display shape,
                // including one diff row and its segments.
                display: Some(ToolResultDisplayDto {
                    headline: Some("Added 1 line".to_string()),
                    headline_kind: Some(HeadlineKindDto::Added),
                    headline_args: vec![1],
                    diff: Some(StructuredDiffDto {
                        file_path: Some("/tmp/example.txt".to_string()),
                        language: Some("txt".to_string()),
                        gutter_width: 1,
                        additions: 1,
                        removals: 0,
                        truncated_rows: 0,
                        rows: vec![DiffRowDto {
                            kind: DiffLineKindDto::Add,
                            line_no: 1,
                            hunk: 0,
                            word_diffed: false,
                            segments: vec![CodeSegmentDto {
                                text: "file body".to_string(),
                                class: SyntaxClassDto::Plain,
                                rgb: Some(0x00c0_c5ce),
                                bold: false,
                                italic: false,
                                underline: false,
                                emph: false,
                            }],
                        }],
                    }),
                    body: Some("file body".to_string()),
                    body_lines: 1,
                    body_truncated: false,
                    collapsed: false,
                }),
            },
        ),
        (
            "event/message_complete.json",
            ClientEvent::MessageComplete {
                stop_reason: Some("end_turn".to_string()),
                message: Some(canonical_message()),
            },
        ),
        (
            "event/turn_started.json",
            ClientEvent::TurnStarted { turn_id: Some(1) },
        ),
        (
            "event/turn_ended.json",
            ClientEvent::TurnEnded {
                outcome: TurnOutcomeDto::EndTurn,
                stop_reason: Some("end_turn".to_string()),
                cost: canonical_cost(),
            },
        ),
        (
            "event/turn_recovery_state.json",
            ClientEvent::TurnRecoveryState {
                snapshot: TurnRecoverySnapshotDto {
                    session_id: "11111111-1111-4111-8111-111111111111".to_string(),
                    turn_id: 1,
                    state: TurnRecoveryStateDto::PausedRecoverable,
                    first_sequence: 1,
                    last_sequence: 7,
                    safe_to_resume: true,
                    reason: Some("background lease expired".to_string()),
                },
            },
        ),
        (
            "event/turn_event_replay.json",
            ClientEvent::TurnEventReplay {
                session_id: "11111111-1111-4111-8111-111111111111".to_string(),
                turn_id: 1,
                sequence: 7,
                event_json: r#"{"type":"text_delta","text":"done"}"#.to_string(),
            },
        ),
        (
            "event/cost_update.json",
            ClientEvent::CostUpdate {
                total_usd: 0.0123,
                input_tokens: 1200,
                output_tokens: 340,
                api_calls: 3,
                session_duration_secs: 42,
                formatted: "$0.0123".to_string(),
            },
        ),
        (
            "event/compaction_status.json",
            ClientEvent::CompactionStatus {
                phase: "summarizing".to_string(),
                error: None,
            },
        ),
        (
            "event/compaction_completed.json",
            ClientEvent::CompactionCompleted {
                messages_before: 50,
                messages_after: 12,
                bytes_saved: 4096,
                summary: "Summary:\nkept context".to_string(),
            },
        ),
        (
            "event/session_started.json",
            ClientEvent::SessionStarted {
                session_id: "11111111-1111-4111-8111-111111111111".to_string(),
                mode: SessionModeDto::Code,
            },
        ),
        ("event/session_ended.json", ClientEvent::SessionEnded),
        (
            "event/session_resumed.json",
            ClientEvent::SessionResumed {
                session_id: "22222222-2222-4222-8222-222222222222".to_string(),
                mode: SessionModeDto::Code,
                // The restored transcript, OLDEST-FIRST. The canonical golden
                // carries a two-message conversation (a user turn + the
                // assistant block set) so the wire shape pins the lowered
                // `MessageDto` element exactly for the client mappers.
                messages: vec![
                    MessageDto {
                        loop_wakeup: None,
                        role: "user".to_string(),
                        blocks: vec![MessageBlockDto::Text {
                            text: "Resume me.".to_string(),
                        }],
                        images: Vec::new(),
                    },
                    canonical_message(),
                ],
            },
        ),
        (
            "event/session_forked.json",
            ClientEvent::SessionForked {
                source_session_id: "22222222-2222-4222-8222-222222222222".to_string(),
                session_id: "33333333-3333-4333-8333-333333333333".to_string(),
                mode: SessionModeDto::Chat,
            },
        ),
        (
            "event/session_agent_list.json",
            ClientEvent::SessionAgentList {
                session_id: "22222222-2222-4222-8222-222222222222".to_string(),
                agents: vec![SessionAgentSummaryDto {
                    agent_id: "main".to_string(),
                    name: "Main agent".to_string(),
                    agent_type: "main".to_string(),
                    model: Some("deepseek-flash".to_string()),
                    model_profile: Some("deepseek".to_string()),
                    status: "running".to_string(),
                    latest_activity: Some("Working on the selected conversation".to_string()),
                    updated_at_ms: Some(1_750_000_000_000),
                }],
            },
        ),
        (
            "event/session_agent_transcript.json",
            ClientEvent::SessionAgentTranscript {
                session_id: "22222222-2222-4222-8222-222222222222".to_string(),
                agent_id: "agent:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string(),
                messages: vec![SessionAgentMessageRowDto {
                    message_index: 0,
                    message_uuid: "44444444-4444-4444-8444-444444444444".into(),
                    message: canonical_message(),
                    api_error_json: None,
                }],
                next_message_index: 1,
                revision: 1,
            },
        ),
        (
            "event/session_agent_updated.json",
            ClientEvent::SessionAgentUpdated {
                session_id: "22222222-2222-4222-8222-222222222222".to_string(),
                agent: SessionAgentSummaryDto {
                    agent_id: "agent:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string(),
                    name: "Researcher".to_string(),
                    agent_type: "general-purpose".to_string(),
                    model: Some("deepseek-flash".to_string()),
                    model_profile: Some("deepseek".to_string()),
                    status: "completed".to_string(),
                    latest_activity: Some("Finished source review".to_string()),
                    updated_at_ms: Some(1_750_000_000_123),
                },
            },
        ),
        (
            "event/session_agent_message.json",
            ClientEvent::SessionAgentMessage {
                session_id: "22222222-2222-4222-8222-222222222222".to_string(),
                agent_id: "agent:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string(),
                row: SessionAgentMessageRowDto {
                    message_index: 0,
                    message_uuid: "44444444-4444-4444-8444-444444444444".into(),
                    message: canonical_message(),
                    api_error_json: None,
                },
            },
        ),
        (
            "event/session_agent_tombstone.json",
            ClientEvent::SessionAgentTombstone {
                session_id: "22222222-2222-4222-8222-222222222222".into(),
                agent_id: "agent:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
                message_uuid: "44444444-4444-4444-8444-444444444444".into(),
                display_only: true,
            },
        ),
        (
            "event/session_list.json",
            ClientEvent::SessionList {
                sessions: vec![SessionRowDto {
                    uuid: "33333333-3333-4333-8333-333333333333".to_string(),
                    title: "Implement the parser".to_string(),
                    modified_rfc3339: "2026-06-02T12:00:00Z".to_string(),
                    message_count: 17,
                    mode: SessionModeDto::Code,
                    path: "/home/dev/.lingxi/sessions/33333333.jsonl".to_string(),
                }],
            },
        ),
        (
            "event/provider_model_catalog.json",
            ClientEvent::ProviderModelCatalog {
                providers: vec![client::protocol::listings::ProviderModelCatalogEntryDto {
                    provider_id: "anthropic".to_string(),
                    provider_label: "Anthropic".to_string(),
                    models: vec![client::protocol::listings::ModelDetailsDto {
                        reference: "anthropic/claude-opus-4-8".to_string(),
                        provider_id: "anthropic".to_string(),
                        provider_label: "Anthropic".to_string(),
                        display_name: "Claude Opus 4.8".to_string(),
                        model_id: "claude-opus-4-8".to_string(),
                        description: Some("Large reasoning model".to_string()),
                        family: Some("claude".to_string()),
                        status: None,
                        release_date: None,
                        last_updated: None,
                        knowledge_cutoff: None,
                        input_modalities: vec!["text".to_string()],
                        output_modalities: vec!["text".to_string()],
                        context_window_tokens: None,
                        max_input_tokens: None,
                        max_output_tokens: None,
                        open_weights: None,
                        attachments: None,
                        temperature_control: None,
                        pricing: None,
                        capabilities: client::protocol::listings::ModelCapabilitiesDto {
                            streaming: true,
                            tools: true,
                            vision: false,
                            documents: false,
                            reasoning: true,
                            structured_output: false,
                        },
                        reasoning: client::protocol::controls::ReasoningControlSpecDto {
                            options: vec![
                                client::protocol::controls::ReasoningOptionDto {
                                    selection:
                                        client::protocol::controls::ReasoningSelectionDto::Automatic,
                                    persistable: true,
                                },
                                client::protocol::controls::ReasoningOptionDto {
                                    selection:
                                        client::protocol::controls::ReasoningSelectionDto::Disabled,
                                    persistable: true,
                                },
                                client::protocol::controls::ReasoningOptionDto {
                                    selection:
                                        client::protocol::controls::ReasoningSelectionDto::Enabled,
                                    persistable: true,
                                },
                                client::protocol::controls::ReasoningOptionDto {
                                    selection:
                                        client::protocol::controls::ReasoningSelectionDto::Level {
                                            id: "low".to_string(),
                                        },
                                    persistable: true,
                                },
                            ],
                            budget_range: None,
                            provider_default:
                                client::protocol::controls::ReasoningSelectionDto::Automatic,
                            forced_reasoning: false,
                            editable: true,
                            disabled_reason: None,
                        },
                        supports_fast_mode: false,
                        fusion_analyst_capable: false,
                    }],
                    group: None, connection_id: None,
                }],
            },
        ),
        (
            "event/model_list.json",
            ClientEvent::ModelList {
                models: vec![
                    "anthropic/claude-opus-4-8".to_string(),
                    "openai/gpt-5.5".to_string(),
                ],
                current: "anthropic/claude-opus-4-8".to_string(),
                details: Vec::new(),
            },
        ),
        (
            "event/model_changed.json",
            ClientEvent::ModelChanged {
                model: "openai/gpt-5.5".to_string(),
            },
        ),
        (
            "event/permission_mode_changed.json",
            ClientEvent::PermissionModeChanged {
                mode: "acceptEdits".to_string(),
            },
        ),
        (
            "event/conversation_controls_changed.json",
            ClientEvent::ConversationControlsChanged {
                controls: canonical_conversation_controls(),
            },
        ),
        (
            "event/fast_mode_changed.json",
            ClientEvent::FastModeChanged { enabled: true },
        ),
        (
            "event/provider_credential_status.json",
            ClientEvent::ProviderCredentialStatus {
                operation_id: 17,
                configured_provider_ids: vec!["deepseek".to_string()],
                unavailable_provider_ids: Vec::new(),
                storage_encrypted: true,
                credential_previews: HashMap::from([(
                    "deepseek".to_string(),
                    "••••cdef".to_string(),
                )]),
                error: None,
            },
        ),
        (
            "event/openai_oauth_updated.json",
            ClientEvent::OpenAiOAuthUpdated {
                session: client::protocol::events::OpenAiOAuthSessionDto {
                    access_token: "access-fixture".into(),
                    refresh_token: Some("refresh-fixture".into()),
                    expires_at: 2_000_000_000,
                    account_id: Some("account-fixture".into()),
                    fedramp: false,
                    email: Some("user@example.com".into()),
                },
            },
        ),
        (
            "event/provider_connection_tested.json",
            ClientEvent::ProviderConnectionTested {
                operation_id: 20,
                provider_id: "deepseek".to_string(),
                connected: true,
                reachable: true,
                authenticated: true,
                model_available: true,
                http_status: Some(200),
                latency_ms: 86,
                message: "连接成功 · 86 ms".to_string(),
                used_stored_credential: true,
            },
        ),
        (
            "event/configuration_operation.json",
            ClientEvent::ConfigurationOperation {
                domain: ConfigurationDomainDto::Plugin,
                operation_id: 41,
                status: ConfigurationOperationStatusDto::Succeeded,
                effect: ConfigurationEffectDto::Applied,
                message: Some("Saved plugin settings.".to_string()),
                details_json: Some(r#"{"scope":"user"}"#.to_string()),
            },
        ),
        (
            "event/skill_catalog.json",
            ClientEvent::SkillCatalog {
                catalog_json: r#"{"skills":[{"id":"user:greet","scope":"user","writable":true}]}"#
                    .to_string(),
            },
        ),
        (
            "event/skill_document.json",
            ClientEvent::SkillDocument {
                document_json: r#"{"id":"user:greet","content":"---\ndescription: greet\n---\n"}"#
                    .to_string(),
            },
        ),
        (
            "event/mcp_configuration_snapshot.json",
            ClientEvent::McpConfigurationSnapshot {
                snapshot_json:
                    r#"{"scopes":[{"scope":"user","raw_json":"{}"}],"runtime_servers":[]}"#
                        .to_string(),
            },
        ),
        (
            "event/plugin_catalog.json",
            ClientEvent::PluginCatalog {
                catalog_json: r#"{"installed":[{"name":"lingxi-local-app","version":"2.0.0"}]}"#
                    .to_string(),
            },
        ),
        (
            "event/mcp_servers.json",
            ClientEvent::McpServers {
                servers: vec![
                    McpServerDto {
                        name: "filesystem".to_string(),
                        status: McpStatusDto::Connected,
                        transport: "stdio".to_string(),
                    },
                    McpServerDto {
                        name: "github".to_string(),
                        status: McpStatusDto::Error {
                            reason: "handshake timeout".to_string(),
                        },
                        transport: "http".to_string(),
                    },
                ],
            },
        ),
        (
            "event/skills.json",
            ClientEvent::Skills {
                skills: vec![
                    SkillDto {
                        name: "greet".to_string(),
                        source_dir: "/home/user/.lingxi/skills/greet".to_string(),
                    },
                    SkillDto {
                        name: "pr-review".to_string(),
                        source_dir: "/repo/.lingxi/skills/pr-review".to_string(),
                    },
                ],
            },
        ),
        (
            "event/typescript_lsp_mode_changed.json",
            ClientEvent::TypescriptLspModeChanged {
                requested: "auto".to_string(),
                effective: "off".to_string(),
                available: false,
            },
        ),
        (
            "event/hooks.json",
            ClientEvent::Hooks {
                hooks: vec![HookDto {
                    name: "format-on-write".to_string(),
                    event: "PostToolUse".to_string(),
                    matcher: Some("Write|Edit".to_string()),
                    timeout_ms: 60_000,
                    hook_type: Some("command".to_string()),
                    source: Some("User settings (~/.lingxi/settings.json)".to_string()),
                    content: Some("./format.sh".to_string()),
                    status_message: Some("Formatting".to_string()),
                    blocking: Some(true),
                    is_async: Some(false),
                    priority: Some(0),
                    async_rewake: Some(false),
                    async_timeout_ms: None,
                    if_condition: Some("Write(*.ts)".to_string()),
                }],
            },
        ),
        (
            "event/agents.json",
            ClientEvent::Agents {
                agents: vec![AgentDto {
                    name: "reviewer".to_string(),
                    description: "Reviews diffs for correctness".to_string(),
                    tools_allowed: vec!["Read".to_string(), "Grep".to_string()],
                }],
            },
        ),
        (
            "event/slash_command_catalog.json",
            ClientEvent::SlashCommandCatalog {
                commands: vec![SlashCommandDto {
                    name: "model".to_string(),
                    description: "Switch the active model".to_string(),
                    source: "builtin".to_string(),
                    aliases: vec!["m".to_string()],
                    argument_hint: Some("[model]".to_string()),
                    menu_description: Some("Switch model".to_string()),
                    hidden: false,
                }],
            },
        ),
        (
            "event/slash_command_result.json",
            ClientEvent::SlashCommandResult {
                turn_id: Some(12),
                display: "Switched model to opus".to_string(),
                is_error: false,
            },
        ),
        (
            "event/memory_entries.json",
            ClientEvent::MemoryEntries {
                entries: vec![MemoryEntryDto {
                    path: "/home/dev/project/LINGXI.md".to_string(),
                    tier: MemoryTierDto::Project,
                    body: "# Project notes".to_string(),
                    age_days: 3,
                    size_bytes: 256,
                }],
            },
        ),
        (
            "event/status_snapshot.json",
            ClientEvent::StatusSnapshot {
                snapshot: canonical_status(),
            },
        ),
        (
            "event/settings_snapshot.json",
            ClientEvent::SettingsSnapshot {
                effective_json: r#"{"model":"claude-opus-4-7"}"#.to_string(),
                provenance_json: r#"{"model":"user-settings"}"#.to_string(),
                // Left `None` deliberately: the golden proves the ADDED
                // optional fields stay off the wire when unset, so a client
                // that predates them sees the byte-identical payload.
                files_json: None,
                active_json: None,
                locked: None,
                layers_json: None,
                merged_keys: None,
            },
        ),
        (
            "event/auth_state.json",
            ClientEvent::AuthState {
                state: AuthStateDto::SignedIn {
                    email: "dev@example.com".to_string(),
                    org_id: "org_abc123".to_string(),
                },
            },
        ),
        (
            "event/doctor_report.json",
            ClientEvent::DoctorReport {
                report: canonical_doctor(),
            },
        ),
        (
            "event/task_row.json",
            ClientEvent::TaskRow {
                task: canonical_task_row(),
            },
        ),
        (
            "event/task_output_chunk.json",
            ClientEvent::TaskOutputChunk {
                task_id: "b12345678".to_string(),
                content: "build output line\n".to_string(),
                total_lines: 128,
                truncated: false,
            },
        ),
        (
            "event/task_status_changed.json",
            ClientEvent::TaskStatusChanged {
                task_id: "b12345678".to_string(),
                status: TaskStatusDto::Running,
                origin_session_id: None,
                error: None,
            },
        ),
        (
            "event/coordinator_status.json",
            ClientEvent::CoordinatorStatus {
                active_workers: 0,
                team: None,
            },
        ),
        (
            "event/coordinator_worker.json",
            ClientEvent::CoordinatorWorker {
                worker: canonical_coordinator_worker(),
            },
        ),
        (
            "event/apps_changed.json",
            ClientEvent::AppsChanged {
                apps: vec![canonical_app_record()],
            },
        ),
        (
            "event/app_details_changed.json",
            ClientEvent::AppEvent {
                event: AppEventDto::AppDetailsChanged {
                    details: canonical_app_details(),
                },
            },
        ),
        (
            "event/app_record_changed.json",
            ClientEvent::AppEvent {
                event: AppEventDto::AppRecordChanged {
                    record: canonical_app_record(),
                },
            },
        ),
        (
            "event/app_profile_proposal.json",
            ClientEvent::AppEvent {
                event: AppEventDto::AppProfileProposal {
                    proposal: AppAgentProfileProposalDto {
                        app_id: "habits-1a2b".to_string(),
                        approval_token: "approval-00000001".to_string(),
                        base_revision: 3,
                        current_revision: 4,
                        instructions: "Prefer compact cards".to_string(),
                        reason: "The user asked for a denser layout".to_string(),
                    },
                },
            },
        ),
        (
            "event/app_workflow_changed.json",
            ClientEvent::AppWorkflowChanged {
                app_id: "habits-1a2b".to_string(),
                state: AppWorkflowStateDto::PublishedUnverified,
                detail: None,
            },
        ),
        (
            "event/app_runtime_changed.json",
            ClientEvent::AppRuntimeChanged {
                app_id: "habits-1a2b".to_string(),
                state: AppRuntimeStateDto::Stopped,
                details: Some(canonical_app_details().runtime),
                last_error: None,
            },
        ),
        (
            "event/app_bridge_response.json",
            ClientEvent::AppEvent {
                event: AppEventDto::AppBridgeResponse {
                    response: AppBridgeResponseDto {
                        request_id: "bridge-00000001".to_string(),
                        app_id: "habits-1a2b".to_string(),
                        ok: true,
                        result_json: Some("[]".to_string()),
                        error: None,
                        error_code: None,
                    },
                },
            },
        ),
        (
            "event/app_ui_request.json",
            ClientEvent::AppEvent {
                event: AppEventDto::AppUiRequest {
                    request: AppUiRequestDto {
                        request_id: "ui-00000001".to_string(),
                        app_id: "habits-1a2b".to_string(),
                        action: AppUiActionKindDto::Inspect,
                        target: None,
                        value: None,
                    },
                },
            },
        ),
        (
            "event/app_capability_requested.json",
            ClientEvent::AppEvent {
                event: AppEventDto::AppCapabilityRequested {
                    request: AppCapabilityRequestDto {
                        request_id: "cap-00000001".to_string(),
                        app_id: "habits-1a2b".to_string(),
                        capability: AppCapabilityKindDto::NetworkDomain,
                        domain: Some("api.example.com".to_string()),
                        reason: "Fetch approved remote data".to_string(),
                    },
                },
            },
        ),
        (
            "event/app_dependency_change_confirmation_requested.json",
            ClientEvent::AppEvent {
                event: AppEventDto::AppDependencyChangeConfirmationRequested {
                    request: AppDependencyChangeConfirmationRequestDto {
                        request_id: "dependency-00000001".to_string(),
                        app_id: "habits-1a2b".to_string(),
                        reason: "pre_resolution_no_network".to_string(),
                        changes: vec![AppDependencyChangeDto {
                            kind: AppDependencyChangeKindDto::Add,
                            package: "dayjs".to_string(),
                            version: Some("1.11.13".to_string()),
                            cache_status: "unknown_until_resolution".to_string(),
                            download_status: "may_be_required".to_string(),
                        }],
                        license_risk: "unknown_until_resolution".to_string(),
                        sbom_risk: "unknown_until_resolution".to_string(),
                        lifecycle_scripts_blocked: true,
                        native_addons_blocked: true,
                        rollback_policy: "rollback_on_validation_failure".to_string(),
                    },
                },
            },
        ),
        (
            "event/app_sessions_changed.json",
            ClientEvent::AppSessionsChanged {
                app_id: "habits-1a2b".to_string(),
                sessions: vec![
                    AppSessionRowDto {
                        uuid: "0f0e0d0c-0b0a-0908-0706-050403020100".to_string(),
                        title: "初始化".to_string(),
                        modified_rfc3339: "2026-08-09T12:00:00Z".to_string(),
                        message_count: 12,
                        mode: SessionModeDto::Code,
                        kind: AppSessionKindDto::Init,
                    },
                    AppSessionRowDto {
                        uuid: "00112233-4455-6677-8899-aabbccddeeff".to_string(),
                        title: "加一个统计页".to_string(),
                        modified_rfc3339: "2026-08-09T13:30:00Z".to_string(),
                        message_count: 7,
                        mode: SessionModeDto::Chat,
                        kind: AppSessionKindDto::Conversation,
                    },
                ],
                next_offset: Some(52),
            },
        ),
        (
            "event/app_checkpoint_created.json",
            ClientEvent::AppCheckpointCreated {
                app_id: "habits-1a2b".to_string(),
                checkpoint: AppCheckpointDto {
                    id: "ckpt-00000001".to_string(),
                    label: "Preview approved".to_string(),
                    kind: AppCheckpointKindDto::PreviewApproved,
                    created_at_ms: 1_750_000_000_000,
                },
            },
        ),
        (
            "event/app_checkpoints_changed.json",
            ClientEvent::AppEvent {
                event: AppEventDto::AppCheckpointsChanged {
                    app_id: "habits-1a2b".to_string(),
                    checkpoints: vec![],
                },
            },
        ),
        // ── PluginStatusChanged (§17.1 / §19.2) ───────────────────────────
        //
        // The READ half of the plugin enable/disable protocol, goldened one
        // row per NESTED `AppEventDto` variant like every `app_event` row
        // above — `every_variant_has_a_golden` compares TOP-LEVEL
        // `ClientEvent` tags only, and `app_event` is already covered, so
        // nothing in the repo would ask for this file. It is here because the
        // convention requires it, not because a gate demanded it.
        //
        // What the canonical instance is contracted to hold:
        //
        // 1. The payload is nested under the single `app_event` envelope, so
        //    `AppEventDto`'s variants stay off `ClientEvent`'s UniFFI enum
        //    metadata budget — the same structural reason the write half is
        //    nested under `ClientCommand::PluginCommand`.
        // 2. `plugin_id` is the BARE `enabledPlugins` key, spelled identically
        //    to the `command/plugin_command_*.json` pair. One name on the
        //    wire, read or write.
        // 3. `state` is a PRESENT, distinctly-tagged value — never `null`,
        //    never omitted. "Explicitly disabled" and "not found" must not
        //    collapse into the same payload (§19.2), which is why `disabled`
        //    is the value pinned here rather than the cheerier `loaded`.
        // 4. `state: "disabled"` is paired with `manifest_default_enabled:
        //    true` ON PURPOSE — the two fields DISAGREE. A golden where they
        //    agreed would still pass if an implementation derived one from
        //    the other; this pair can only be produced by carrying both
        //    across the wire independently, which is the actual contract
        //    (an explicit override beats the manifest default).
        (
            "event/plugin_status_changed.json",
            ClientEvent::AppEvent {
                event: AppEventDto::PluginStatusChanged {
                    status: PluginStatusDto {
                        plugin_id: "lingxi-local-app".to_string(),
                        state: PluginActivationStateDto::Disabled,
                        manifest_default_enabled: true,
                    },
                },
            },
        ),
        (
            "event/plugin_inventory_changed.json",
            ClientEvent::AppEvent {
                event: AppEventDto::PluginInventoryChanged {
                    inventory: LocalAppPluginInventoryDto {
                        plugin_id: "lingxi-local-app".to_string(),
                        display_name: "Local App Plugin".to_string(),
                        source: "builtin".to_string(),
                        version: "2.0.0-dev".to_string(),
                        bundle_sha256: "a".repeat(64),
                        state: PluginActivationStateDto::Loaded,
                        manifest_default_enabled: true,
                        counts: LocalAppPluginComponentCountsDto {
                            skills: 27,
                            agents: 1,
                            workflows: 6,
                            templates: 4,
                        },
                        validation_error: Some(
                            "The verified builtin bundle root is missing.".to_string(),
                        ),
                    },
                },
            },
        ),
        (
            "event/create_confirmation_requested.json",
            ClientEvent::AppEvent {
                event: AppEventDto::CreateConfirmationRequested {
                    request: LocalAppCreateConfirmationRequestDto {
                        request_id: "create-0001".to_string(),
                        app_id: "habits-1a2b".to_string(),
                        name: "Habits".to_string(),
                        brief: "Track streaks and notes".to_string(),
                        selected_template: LocalAppTemplateSummaryDto {
                            template_id: "react-dom-r1".to_string(),
                            surface: AppSurfaceDto::Dom,
                            summary: "Best for forms and lists".to_string(),
                        },
                        runtime_profile: canonical_local_app_profile(),
                        reason: "The user asked for a compact habit list.".to_string(),
                        rejected: vec![LocalAppRejectedCandidateDto {
                            template_id: "three-3d-r1".to_string(),
                            reason: "3D is unnecessary for this brief.".to_string(),
                        }],
                        initial_tools: vec![canonical_local_app_tool("save_habit")],
                        required_gates: vec![canonical_local_app_gate()],
                    },
                },
            },
        ),
        (
            "event/mcp_proposal_approval_requested.json",
            ClientEvent::AppEvent {
                event: AppEventDto::McpProposalApprovalRequested {
                    request: LocalAppMcpProposalApprovalRequestDto {
                        request_id: "proposal-0001".to_string(),
                        app_id: "habits-1a2b".to_string(),
                        workflow_run_id: "wf-0002".to_string(),
                        summary: "Add save_habit and remove summarize_habits".to_string(),
                        proposal_sha256: "3".repeat(64),
                        approval_contract_sha256: "4".repeat(64),
                        tool_surface_sha256: "5".repeat(64),
                        tool_diffs: vec![
                            LocalAppMcpToolDiffDto {
                                kind: LocalAppMcpToolChangeKindDto::Removed,
                                name: "summarize_habits".to_string(),
                                before: Some(canonical_local_app_tool("summarize_habits")),
                                after: None,
                                changed_fields: vec![],
                            },
                            LocalAppMcpToolDiffDto {
                                kind: LocalAppMcpToolChangeKindDto::Changed,
                                name: "save_habit".to_string(),
                                before: Some(canonical_local_app_tool("save_habit")),
                                after: Some(LocalAppMcpToolSurfaceDto {
                                    description: Some(
                                        "Create or update one completed-habits entry.".to_string(),
                                    ),
                                    ..canonical_local_app_tool("save_habit")
                                }),
                                changed_fields: vec![
                                    LocalAppMcpToolFieldDto::Description,
                                    LocalAppMcpToolFieldDto::InputSchema,
                                    LocalAppMcpToolFieldDto::PermissionCeiling,
                                ],
                            },
                        ],
                        required_flow_changes: vec!["Add a save step for notes.".to_string()],
                        excluded_capabilities: vec!["calendar".to_string()],
                        pending_gates: vec![canonical_local_app_gate()],
                    },
                },
            },
        ),
        (
            "event/managed_mcp_inventory_changed.json",
            ClientEvent::AppEvent {
                event: AppEventDto::ManagedMcpInventoryChanged {
                    servers: vec![ManagedLocalAppMcpServerDto {
                        server_name: "local_app_habits-1a2b".to_string(),
                        app_id: "habits-1a2b".to_string(),
                        app_name: "Habits".to_string(),
                        enabled: true,
                        status: ManagedLocalAppMcpStatusDto::Enabled,
                        settings_revision: 6,
                        enabled_tools: vec!["save_habit".to_string()],
                        pinned_to_current_conversation: true,
                        build_id: "build-0001".to_string(),
                        catalog_sha256: "6".repeat(64),
                        tool_surface_sha256: "7".repeat(64),
                        tool_count: 2,
                        authoring_revision: 3,
                        publication_state: AppWorkflowStateDto::PublishedUnverified,
                        mcp_verification: LocalAppVerificationSummaryDto {
                            status: LocalAppVerificationStatusDto::Passed,
                            summary: "MCP schema, binding and isolation checks passed.".to_string(),
                            code: None,
                        },
                        ui_verification: LocalAppVerificationSummaryDto {
                            status: LocalAppVerificationStatusDto::Unavailable,
                            summary: "UI verification runner is unavailable.".to_string(),
                            code: Some("verification_unavailable".to_string()),
                        },
                        widget: Some(canonical_local_app_widget()),
                        tools: vec![canonical_local_app_tool("save_habit")],
                    }],
                },
            },
        ),
        (
            "event/verification_summary_changed.json",
            ClientEvent::AppEvent {
                event: AppEventDto::VerificationSummaryChanged {
                    app_id: "habits-1a2b".to_string(),
                    publication_state: AppWorkflowStateDto::PublishedVerified,
                    mcp_verification: LocalAppVerificationSummaryDto {
                        status: LocalAppVerificationStatusDto::Passed,
                        summary: "Catalog and MCP verification are current.".to_string(),
                        code: None,
                    },
                    ui_verification: LocalAppVerificationSummaryDto {
                        status: LocalAppVerificationStatusDto::Unverified,
                        summary: "UI verification has not run on this build.".to_string(),
                        code: None,
                    },
                },
            },
        ),
        (
            "event/local_app_operation_failed.json",
            ClientEvent::AppEvent {
                event: AppEventDto::LocalAppOperationFailed {
                    app_id: None,
                    code: LocalAppPluginErrorCodeDto::BuiltinBundleUnavailable,
                    message: "The verified builtin bundle root is missing.".to_string(),
                    request_id: Some("plugin-read-1".to_string()),
                },
            },
        ),
        (
            "event/app_operation_failed.json",
            ClientEvent::AppOperationFailed {
                app_id: Some("habits-1a2b".to_string()),
                code: AppErrorCodeDto::NotYetAvailable,
                message: "app runtime lands in phase 4".to_string(),
                // Non-default on purpose: a `None` here would drop the key
                // from the golden, so a rename of the field could not be
                // caught by this snapshot.
                request_id: Some("2f1e0d9c-8b7a-4655-9443-2211ffee0099".to_string()),
            },
        ),
        (
            "event/thinking_delta.json",
            ClientEvent::ThinkingDelta {
                thinking: "Let me reason about this.".to_string(),
                signature: Some("sig_abc".to_string()),
            },
        ),
        (
            "event/usage_update.json",
            ClientEvent::UsageUpdate {
                is_snapshot: None,
                input_tokens: 1200,
                output_tokens: 340,
                cache_read_tokens: 800,
                cache_creation_tokens: 64,
            },
        ),
        (
            "event/api_retry.json",
            ClientEvent::ApiRetry {
                message: "rate limited".to_string(),
                attempt: 2,
                max_retries: 5,
                delay_ms: 1_000,
            },
        ),
        (
            "event/ask_user_question.json",
            ClientEvent::AskUserQuestion {
                request: AskUserQuestionRequestDto {
                    request_id: 9,
                    questions: vec![AskQuestionDto {
                        question: "Which database should the app use?".to_string(),
                        header: "Storage".to_string(),
                        options: vec![AskOptionDto {
                            label: "SQLite".to_string(),
                            description: "Local, file-backed, zero setup.".to_string(),
                            preview: Some("app.sqlite".to_string()),
                        }],
                        multi_select: false,
                    }],
                    timeout_secs: Some(120),
                },
            },
        ),
        (
            "event/ask_user_question_resolved.json",
            ClientEvent::AskUserQuestionResolved { request_id: 9 },
        ),
        (
            "event/permission_request_resolved.json",
            ClientEvent::PermissionRequestResolved {
                request_id: 10,
                resolution: PermissionResolutionDto::Expired,
            },
        ),
        (
            "event/commands_changed.json",
            ClientEvent::CommandsChanged {
                commands: vec![SlashCommandDto {
                    name: "compact".to_string(),
                    description: "Compact the conversation history.".to_string(),
                    source: "builtin".to_string(),
                    aliases: vec!["cmp".to_string()],
                    argument_hint: None,
                    menu_description: Some("Compact chat".to_string()),
                    hidden: false,
                }],
            },
        ),
        (
            "event/attachment.json",
            ClientEvent::Attachment {
                attachment: AttachmentDto::NestedMemory {
                    display_path: "src/LINGXI.md".to_string(),
                },
            },
        ),
        (
            "event/audio_request.json",
            ClientEvent::AudioRequest {
                request: AudioOperationRequestDto {
                    identity: AudioOperationIdDto {
                        id: "00000000-0000-4000-8000-000000000007".to_string(),
                        generation: 2,
                        service_epoch: 3,
                    },
                    owner: AudioOwnerDto::Session {
                        session_id: "session-1".to_string(),
                    },
                    initiator: None,
                    timeout_budget_ms: Some(1_000),
                    max_payload_bytes: 12_533_760,
                    operation: AudioOperationDto::Listen {
                        language: Some("zh-CN".to_string()),
                    },
                },
            },
        ),
        (
            "event/audio_cancel.json",
            ClientEvent::AudioCancel {
                identity: AudioOperationIdDto {
                    id: "00000000-0000-4000-8000-000000000007".to_string(),
                    generation: 2,
                    service_epoch: 3,
                },
            },
        ),
        (
            "event/audio_capabilities_changed.json",
            ClientEvent::AudioCapabilitiesChanged {
                capabilities: AudioCapabilitySnapshotDto {
                    service_epoch: 3,
                    support_revision: 2,
                    supported_operations: vec![
                        AudioOperationKindDto::Listen,
                        AudioOperationKindDto::Speak,
                    ],
                    readiness: vec![AudioOperationReadinessDto {
                        operation: AudioOperationKindDto::Listen,
                        state: AudioReadinessStateDto::NeedsPermission,
                    }],
                    max_payload_bytes: 12_533_760,
                },
            },
        ),
        (
            "event/query_model_change.json",
            ClientEvent::QueryModelChange {
                to_model: "claude-sonnet-4".into(),
            },
        ),
        (
            "event/assistant_block_start.json",
            ClientEvent::AssistantBlockStart { block_key: 7 },
        ),
        (
            "event/assistant_block_identity.json",
            ClientEvent::AssistantBlockIdentity {
                block_key: 7,
                message_uuid: "assistant-row-1".into(),
            },
        ),
        (
            "event/user_transcript_row_identity.json",
            ClientEvent::UserTranscriptRowIdentity {
                row_token: "pending-user-row".into(),
                uuid: "persisted-user-row".into(),
            },
        ),
        (
            "event/assistant_transcript_row_uuids.json",
            ClientEvent::AssistantTranscriptRowUuids {
                message_id: "assistant-turn-1".into(),
                uuids: vec![Some("assistant-text-row-1".into()), None],
            },
        ),
        (
            "event/tombstone.json",
            ClientEvent::Tombstone {
                message: ServerFallbackTombstoneMessageDto {
                    uuid: "assistant-row-1".into(),
                    message_type: "assistant".into(),
                    timestamp: "2026-10-03T12:00:00.000Z".into(),
                    request_id: Some("request-1".into()),
                    request_ref_json: Some(r#"{"route":"native"}"#.into()),
                    message: ServerFallbackProviderMessageDto {
                        id: Some("provider-message-1".into()),
                        model: Some("claude-opus-4".into()),
                        stop_reason: Some("tool_use".into()),
                        stop_details_json: Some(r#"{"category":"cyber"}"#.into()),
                        usage_json: Some(r#"{"input_tokens":5,"output_tokens":2}"#.into()),
                        content_json: r#"[{"type":"text","text":"superseded"}]"#.into(),
                    },
                    is_api_error_message: Some(false),
                    supersedes_uuids: Some(vec!["assistant-row-0".into()]),
                },
                display_only: true,
            },
        ),
        (
            "event/refusal_continuation.json",
            ClientEvent::RefusalContinuation {
                phase: RefusalContinuationPhaseDto::Begin,
                salvage_text: "retained refusal text".into(),
                join: RefusalContinuationJoinDto::Exact,
                replaces_uuids: vec!["assistant-row-0".into()],
                display_salvage_text: true,
            },
        ),
    ]
}

/// Every `ClientCommand` variant, paired with its golden filename.
#[allow(clippy::too_many_lines)]
fn command_goldens() -> Vec<(&'static str, ClientCommand)> {
    vec![
        ("command/cron_run_started.json", ClientCommand::CronRunStarted {run_id: "run-1".into(), session_id: "session-1".into()}),
        ("command/cron_run_completed.json", ClientCommand::CronRunCompleted {run_id: "run-1".into(), session_id: Some("session-1".into()), summary: Some("Completed".into()), error: None}),
        ("command/scheduled_run_turn.json", ClientCommand::ScheduledRunTurn {run_id: "run-1".into(), prompt: "Daily brief".into(), model: "openai/model".into(), reasoning: ReasoningSelectionDto::Automatic}),
        ("command/task_message.json", ClientCommand::TaskMessage { task_id: "a12345678".into(), message: "Please continue the review.".into() }),
        ("command/cron_manage.json", ClientCommand::CronManage {
            request_id: "cron-1".into(),
            request: client::protocol::commands::CronRequestDto {
            automation: None,
                action: "list".into(), id: None, cron: None, prompt: None,
                recurring: None, durable: None, expires_at: None, no_expiry: None,
            },
        }),
        (
            "command/send_prompt.json",
            ClientCommand::SendPrompt {
                text: "summarize the diff".to_string(),
                prompt_mode: Some(PromptModeDto::Normal),
                images: vec![ImageRefDto {
                    media_type: "image/png".to_string(),
                    base64: "iVBORw0KGgo=".to_string(),
                }],
                turn_id: Some(1),
            },
        ),
        (
            "command/cancel.json",
            ClientCommand::Cancel { turn_id: Some(1) },
        ),
        (
            "command/attach_turn.json",
            ClientCommand::AttachTurn {
                turn_id: 1,
                after_sequence: Some(4),
            },
        ),
        (
            "command/resume_turn.json",
            ClientCommand::ResumeTurn { turn_id: 1 },
        ),
        (
            "command/pause_turn.json",
            ClientCommand::PauseTurn {
                turn_id: 1,
                reason: "background_time_expired".to_string(),
            },
        ),
        (
            "command/set_typescript_lsp_mode.json",
            ClientCommand::SetTypescriptLspMode {
                mode: "auto".to_string(),
            },
        ),
        (
            "command/approve_permission.json",
            ClientCommand::ApprovePermission {
                request_id: 7,
                response: PermissionResponseDto::AllowOnce,
            },
        ),
        (
            "command/deny_permission.json",
            ClientCommand::DenyPermission { request_id: 7 },
        ),
        (
            "command/approve_computer_access.json",
            ClientCommand::ApproveComputerAccess {
                request_id: 42,
                response: ComputerAccessResponseDto {
                    granted_apps: vec!["Slack".to_string()],
                    clipboard_read: false,
                    clipboard_write: false,
                    system_key_combos: false,
                },
            },
        ),
        (
            "command/deny_computer_access.json",
            ClientCommand::DenyComputerAccess { request_id: 42 },
        ),
        (
            "command/set_permission_mode.json",
            ClientCommand::SetPermissionMode {
                mode: "acceptEdits".to_string(),
            },
        ),
        (
            "command/list_provider_credentials.json",
            ClientCommand::ListProviderCredentials {
                operation_id: 17,
                provider_ids: vec!["deepseek".to_string(), "openrouter".to_string()],
                preview_provider_ids: Vec::new(),
            },
        ),
        (
            "command/set_provider_credential.json",
            ClientCommand::SetProviderCredential {
                operation_id: 18,
                provider_id: "deepseek".to_string(),
                credential: ProviderCredentialSecretDto::new("sk-redacted-snapshot".to_string()),
            },
        ),
        (
            "command/delete_provider_credential.json",
            ClientCommand::DeleteProviderCredential {
                operation_id: 19,
                provider_id: "deepseek".to_string(),
            },
        ),
        (
            "command/test_provider_connection.json",
            ClientCommand::TestProviderConnection {
                operation_id: 20,
                provider_id: "deepseek".to_string(),
                api_base: "https://api.deepseek.com".to_string(),
                model: "deepseek-flash".to_string(),
                credential_override: None,
            },
        ),
        (
            "command/set_model.json",
            ClientCommand::SetModel {
                model: "claude-sonnet-4-5".to_string(),
            },
        ),
        ("command/list_models.json", ClientCommand::ListModels),
        (
            "command/get_conversation_controls.json",
            ClientCommand::GetConversationControls,
        ),
        (
            "command/set_reasoning_selection.json",
            ClientCommand::SetReasoningSelection {
                selection: ReasoningSelectionDto::TokenBudget { tokens: 2048 },
            },
        ),
        (
            "command/set_fast_mode.json",
            ClientCommand::SetFastMode { enabled: true },
        ),
        (
            "command/run_slash_command.json",
            ClientCommand::RunSlashCommand {
                raw: "/model opus".to_string(),
                turn_id: Some(7),
            },
        ),
        (
            "command/refresh_listings.json",
            ClientCommand::RefreshListings {
                which: vec![ListingKindDto::Mcp, ListingKindDto::Agents],
            },
        ),
        (
            "command/list_session_agents.json",
            ClientCommand::ListSessionAgents,
        ),
        (
            "command/load_session_agent_transcript.json",
            ClientCommand::LoadSessionAgentTranscript {
                agent_id: "agent:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string(),
            },
        ),
        (
            "command/new_session.json",
            ClientCommand::NewSession {
                cwd: Some("/home/dev/project".to_string()),
                model: Some("claude-opus-4-7".to_string()),
            },
        ),
        (
            "command/resume_session.json",
            ClientCommand::ResumeSession {
                session_id: "44444444-4444-4444-8444-444444444444".to_string(),
                cwd: Some("/home/dev/project".to_string()),
            },
        ),
        (
            "command/list_sessions.json",
            ClientCommand::ListSessions { limit: Some(20) },
        ),
        (
            "command/fork_session.json",
            ClientCommand::ForkSession {
                session_id: "44444444-4444-4444-8444-444444444444".to_string(),
                target_mode: SessionModeDto::Chat,
            },
        ),
        ("command/login.json", ClientCommand::Login),
        ("command/logout.json", ClientCommand::Logout),
        ("command/force_compact.json", ClientCommand::ForceCompact),
        ("command/clear_session.json", ClientCommand::ClearSession),
        (
            "command/ui_attach.json",
            ClientCommand::UiAttach {
                surface: UiSurfaceDto::Desktop,
                client_id: "electron-window-1".to_string(),
            },
        ),
        (
            "command/ui_detach.json",
            ClientCommand::UiDetach {
                client_id: "electron-window-1".to_string(),
            },
        ),
        (
            "command/ui_render.json",
            ClientCommand::UiRender {
                request_id: "render-1".into(),
                request_json: r#"{"subtype":"ui_render"}"#.into(),
            },
        ),
        (
            "command/ui_client_module.json",
            ClientCommand::UiClientModule {
                request_id: "module-1".into(),
                plugin: "review".into(),
            },
        ),
        (
            "command/ui_message.json",
            ClientCommand::UiMessage {
                request_id: "message-1".into(),
                request_json: r#"{"subtype":"ui_message"}"#.into(),
            },
        ),
        (
            "command/ui_client_fault.json",
            ClientCommand::UiClientFault {
                request_id: "fault-1".into(),
                request_json: r#"{"subtype":"ui_client_fault"}"#.into(),
            },
        ),
        (
            "command/ui_client_press.json",
            ClientCommand::UiClientPress {
                request_id: "press-1".into(),
                request_json: r#"{"subtype":"ui_client_press"}"#.into(),
            },
        ),
        (
            "command/ui_press.json",
            ClientCommand::UiPress {
                request_id: "parent-press-1".into(),
                request_json: r#"{"subtype":"ui_press"}"#.into(),
            },
        ),
        (
            "command/ui_input.json",
            ClientCommand::UiInput {
                request_id: "parent-input-1".into(),
                request_json: r#"{"subtype":"ui_input"}"#.into(),
            },
        ),
        (
            "command/ui_select.json",
            ClientCommand::UiSelect {
                request_id: "parent-select-1".into(),
                request_json: r#"{"subtype":"ui_select"}"#.into(),
            },
        ),
        (
            "command/ui_client_operation.json",
            ClientCommand::UiClientOperation {
                request_id: "operation-1".into(),
                operation_json: r#"{"subtype":"mount"}"#.into(),
            },
        ),
        (
            "command/task_list.json",
            ClientCommand::TaskList {
                request_id: None,
                status_filter: Some(TaskStatusDto::Running),
            },
        ),
        (
            "command/task_output.json",
            ClientCommand::TaskOutput {
                task_id: "b12345678".to_string(),
                offset: 0,
            },
        ),
        (
            "command/task_stop.json",
            ClientCommand::TaskStop {
                task_id: "b12345678".to_string(),
            },
        ),
        (
            "command/resume_workflow.json",
            ClientCommand::ResumeWorkflow {
                task_id: "w12345678".to_string(),
            },
        ),
        ("command/list_apps.json", ClientCommand::ListApps),
        (
            "command/get_app_details.json",
            ClientCommand::GetAppDetails {
                app_id: "habits-1a2b".to_string(),
            },
        ),
        (
            "command/create_app.json",
            ClientCommand::CreateApp {
                name: "Habits".to_string(),
                origin: AppCreateOriginDto::Chat,
                brief: "Track daily habits with streaks".to_string(),
                git_enabled: true,
                workflow_model: Some("deepseek/deepseek-flash".to_string()),
                conversation_id: Some("55555555-5555-4555-8555-555555555555".to_string()),
                // Non-default on purpose: `Dom` is what an absent surface means,
                // so a golden using it would freeze a payload in which the field
                // never appears and could not catch a rename of it.
                surface: Some(AppSurfaceDto::Canvas),
                mode: AppCreateModeDto::Scaffolded,
                // Non-default on purpose, for the same reason as `surface`:
                // a `None` would drop the key from the golden entirely.
                request_id: Some("2f1e0d9c-8b7a-4655-9443-2211ffee0099".to_string()),
            },
        ),
        (
            "command/start_app.json",
            ClientCommand::StartApp {
                app_id: "habits-1a2b".to_string(),
            },
        ),
        (
            "command/stop_app.json",
            ClientCommand::StopApp {
                app_id: "habits-1a2b".to_string(),
            },
        ),
        (
            "command/restart_app.json",
            ClientCommand::RestartApp {
                app_id: "habits-1a2b".to_string(),
            },
        ),
        (
            "command/execute_app_bridge_request.json",
            ClientCommand::ExecuteAppBridgeRequest {
                request: AppBridgeRequestDto {
                    request_id: "bridge-00000001".to_string(),
                    app_id: "habits-1a2b".to_string(),
                    operation: AppBridgeOperationDto::QueryData,
                    payload_json: Some(r#"{"collection":"items"}"#.to_string()),
                },
            },
        ),
        (
            "command/resolve_app_ui_request.json",
            ClientCommand::ResolveAppUiRequest {
                request_id: "ui-00000001".to_string(),
                decision: AppAuthorizationDecisionDto::AllowSession,
                result_json: Some(r#"{"elements":[]}"#.to_string()),
                error: None,
            },
        ),
        (
            "command/resolve_app_capability_request.json",
            ClientCommand::ResolveAppCapabilityRequest {
                request_id: "cap-00000001".to_string(),
                decision: AppAuthorizationDecisionDto::AllowAlways,
            },
        ),
        (
            "command/resolve_app_dependency_change_confirmation.json",
            ClientCommand::ResolveAppDependencyChangeConfirmation {
                request_id: "dependency-00000001".to_string(),
                approved: true,
            },
        ),
        (
            "command/resolve_app_profile_proposal.json",
            ClientCommand::ResolveAppProfileProposal {
                app_id: "habits-1a2b".to_string(),
                approval_token: "approval-00000001".to_string(),
                approved: true,
            },
        ),
        (
            "command/resolve_app_runtime_profile_selection.json",
            ClientCommand::ResolveAppRuntimeProfileSelection {
                request_id: "runtime-00000001".to_string(),
                selected_family: AppRuntimeProfileDto::ReactDom,
            },
        ),
        (
            "command/reset_app_permissions.json",
            ClientCommand::ResetAppPermissions {
                app_id: "habits-1a2b".to_string(),
            },
        ),
        (
            "command/list_app_sessions.json",
            ClientCommand::ListAppSessions {
                app_id: "habits-1a2b".to_string(),
                offset: Some(50),
                limit: Some(50),
            },
        ),
        (
            "command/list_app_checkpoints.json",
            ClientCommand::ListAppCheckpoints {
                app_id: "habits-1a2b".to_string(),
            },
        ),
        (
            "command/restore_app_checkpoint.json",
            ClientCommand::RestoreAppCheckpoint {
                app_id: "habits-1a2b".to_string(),
                checkpoint_id: "ckpt-00000001".to_string(),
            },
        ),
        (
            "command/delete_app.json",
            ClientCommand::DeleteApp {
                app_id: "habits-1a2b".to_string(),
            },
        ),
        (
            // Exactly ONE answer entry: `answers` is a `HashMap`, which
            // `to_string_pretty` writes in (randomly seeded) iteration order —
            // two or more entries would make the golden bytes non-deterministic.
            "command/answer_ask_user_question.json",
            ClientCommand::AnswerAskUserQuestion {
                request_id: 9,
                answers: HashMap::from([(
                    "Which database should the app use?".to_string(),
                    "SQLite".to_string(),
                )]),
            },
        ),
        (
            "command/cancel_ask_user_question.json",
            ClientCommand::CancelAskUserQuestion { request_id: 9 },
        ),
        ("command/request_exit.json", ClientCommand::RequestExit),
        // ── PluginCommand (§17.1 / §19.2) ─────────────────────────────────
        //
        // TWO rows for ONE `ClientCommand` variant, on purpose.
        //
        // `every_variant_has_a_golden` compares TOP-LEVEL tags only, so a
        // single `plugin_command` row satisfies it while leaving the nested
        // `PluginCommandDto` — where every field of this feature actually
        // lives — entirely unpinned. That is the same blind spot the
        // `AppEventDto` rows above already work around: they are goldened one
        // row per NESTED variant (`event/app_details_changed.json`, …), never
        // one row for the `app_event` envelope. These follow that convention.
        //
        // What the pair is contracted to hold, beyond "whatever serde emits":
        //
        // 1. The operation is a nested OBJECT under `command`, never
        //    `#[serde(flatten)]`ed onto the envelope. Flattening would move
        //    §17.1's operations onto `ClientCommand`'s 16 KiB UniFFI metadata
        //    budget (see `CLIENT_COMMAND_METADATA_BUDGET`), which detonates at
        //    const-eval on both mobile builds. The `command` sub-object in
        //    both goldens is what makes that structural choice byte-visible.
        // 2. `plugin_id` is the BARE `enabledPlugins` key — no `@marketplace`
        //    suffix — and it is spelled identically on the write path
        //    (`set_enabled`) and the read path (`get_status`). One name on the
        //    wire, read or write (`PluginStatusDto::plugin_id`).
        // 3. `set_enabled` carries exactly `{plugin_id, enabled}`. There is no
        //    companion "override" / "use default" flag: writing `enabled` IS
        //    the only way to toggle, and `manifest_default_enabled` is
        //    reported back on the event, never sent up. A third key appearing
        //    here is a contract change, not a detail.
        // 4. `get_status` carries exactly `{plugin_id}` — a pure read with no
        //    payload; its answer arrives as `AppEventDto::PluginStatusChanged`.
        (
            "command/plugin_command_set_enabled.json",
            ClientCommand::PluginCommand {
                command: PluginCommandDto::SetEnabled {
                    plugin_id: "lingxi-local-app".to_string(),
                    enabled: true,
                },
            },
        ),
        (
            "command/plugin_command_get_status.json",
            ClientCommand::PluginCommand {
                command: PluginCommandDto::GetStatus {
                    plugin_id: "lingxi-local-app".to_string(),
                },
            },
        ),
        (
            "command/plugin_command_get_inventory.json",
            ClientCommand::PluginCommand {
                command: PluginCommandDto::GetInventory {
                    plugin_id: "lingxi-local-app".to_string(),
                },
            },
        ),
        (
            "command/plugin_command_resolve_create_confirmation.json",
            ClientCommand::PluginCommand {
                command: PluginCommandDto::ResolveCreateConfirmation {
                    request_id: "create-0001".to_string(),
                    approved: true,
                },
            },
        ),
        (
            "command/plugin_command_resolve_mcp_proposal_approval.json",
            ClientCommand::PluginCommand {
                command: PluginCommandDto::ResolveMcpProposalApproval {
                    request_id: "proposal-0001".to_string(),
                    approved: false,
                },
            },
        ),
        (
            "command/plugin_command_start_local_app_mcp_authoring.json",
            ClientCommand::PluginCommand {
                command: PluginCommandDto::StartLocalAppMcpAuthoring {
                    app_id: "habits-1a2b".to_string(),
                    user_goal: "Let the model save and summarize my habit data.".to_string(),
                },
            },
        ),
        (
            "command/plugin_command_set_local_app_mcp_enabled.json",
            ClientCommand::PluginCommand {
                command: PluginCommandDto::SetLocalAppMcpEnabled {
                    app_id: "habits-1a2b".to_string(),
                    enabled: true,
                    expected_revision: 4,
                },
            },
        ),
        (
            "command/plugin_command_set_local_app_mcp_tool_enabled.json",
            ClientCommand::PluginCommand {
                command: PluginCommandDto::SetLocalAppMcpToolEnabled {
                    app_id: "habits-1a2b".to_string(),
                    tool_name: "save_habit".to_string(),
                    enabled: false,
                    expected_revision: 5,
                },
            },
        ),
        (
            "command/plugin_command_set_local_app_mcp_conversation_pinned.json",
            ClientCommand::PluginCommand {
                command: PluginCommandDto::SetLocalAppMcpConversationPinned {
                    conversation_id: "conv-0001".to_string(),
                    app_id: "habits-1a2b".to_string(),
                    pinned: true,
                },
            },
        ),
        (
            "command/plugin_command_get_managed_mcp_inventory.json",
            ClientCommand::PluginCommand {
                command: PluginCommandDto::GetManagedMcpInventory,
            },
        ),
        (
            "command/update_settings.json",
            ClientCommand::UpdateSettings {
                destination: WritableScopeDto::User,
                patch_json: r#"{"outputStyle":"terse"}"#.to_string(),
            },
        ),
        (
            "command/update_permission_rules.json",
            ClientCommand::UpdatePermissionRules {
                destination: WritableScopeDto::Project,
                behavior: PermissionBehaviorDto::Allow,
                add: vec!["Bash(ls:*)".to_string()],
                remove: vec![],
            },
        ),
        (
            "command/set_default_permission_mode.json",
            ClientCommand::SetDefaultPermissionMode {
                destination: WritableScopeDto::User,
                mode: "acceptEdits".to_string(),
            },
        ),
        (
            "command/update_workspace_directories.json",
            ClientCommand::UpdateWorkspaceDirectories {
                destination: WritableScopeDto::Local,
                add: vec!["/tmp/extra".to_string()],
                remove: vec![],
            },
        ),
        (
            "command/upsert_mcp_server.json",
            ClientCommand::UpsertMcpServer {
                scope: WritableScopeDto::Project,
                name: "linear".to_string(),
                config_json: r#"{"command":"npx","args":["-y","linear-mcp"]}"#.to_string(),
            },
        ),
        (
            "command/remove_mcp_server.json",
            ClientCommand::RemoveMcpServer {
                scope: WritableScopeDto::User,
                name: "linear".to_string(),
            },
        ),
        (
            "command/skill_admin.json",
            ClientCommand::SkillAdmin {
                command: SkillAdminCommandDto {
                    action: "save_document".to_string(),
                    operation_id: Some(31),
                    target: None,
                    scope: None,
                    revision: Some("a".repeat(64)),
                    payload_json: Some(
                        r#"{"skill_id":"/home/dev/.lingxi/skills/greet","content":"---\ndescription: greet\n---\n"}"#
                            .to_string(),
                    ),
                },
            },
        ),
        (
            "command/mcp_admin.json",
            ClientCommand::McpAdmin {
                command: McpAdminCommandDto {
                    action: "save_server".to_string(),
                    operation_id: Some(32),
                    target: None,
                    scope: None,
                    revision: Some("b".repeat(64)),
                    payload_json: Some(
                        r#"{"scope":"project","name":"filesystem","config":{"command":"npx","args":["-y","@modelcontextprotocol/server-filesystem"]}}"#
                            .to_string(),
                    ),
                },
            },
        ),
        (
            "command/plugin_admin.json",
            ClientCommand::PluginAdmin {
                command: PluginAdminCommandDto {
                    action: "preview_operation".to_string(),
                    operation_id: Some(33),
                    target: None,
                    scope: None,
                    revision: Some("c".repeat(64)),
                    payload_json: Some(
                        r#"{"action":"install","plugin_id":"lingxi-local-app"}"#.to_string(),
                    ),
                },
            },
        ),
        (
            "command/hook_admin.json",
            ClientCommand::HookAdmin {
                command: HookAdminCommandDto {
                    action: "save_document".to_string(),
                    operation_id: Some(34),
                    target: None,
                    scope: None,
                    revision: Some("d".repeat(64)),
                    payload_json: Some(
                        r#"{"scope":"local","hooks":{"preToolUse":[]}}"#.to_string(),
                    ),
                },
            },
        ),
        (
            "command/audio_response.json",
            ClientCommand::AudioResponse {
                identity: AudioOperationIdDto {
                    id: "00000000-0000-4000-8000-000000000007".to_string(),
                    generation: 2,
                    service_epoch: 3,
                },
                result: AudioOperationResultDto::Transcript {
                    text: "你好".to_string(),
                    language: Some("zh-CN".to_string()),
                    confidence: Some(0.9),
                },
            },
        ),
        (
            "command/update_audio_capabilities.json",
            ClientCommand::UpdateAudioCapabilities {
                capabilities: AudioCapabilitySnapshotDto {
                    service_epoch: 3,
                    support_revision: 2,
                    supported_operations: vec![
                        AudioOperationKindDto::Listen,
                        AudioOperationKindDto::Speak,
                    ],
                    readiness: vec![AudioOperationReadinessDto {
                        operation: AudioOperationKindDto::Listen,
                        state: AudioReadinessStateDto::NeedsPermission,
                    }],
                    max_payload_bytes: 12_533_760,
                },
            },
        ),
    ]
}

// ─────────────────────────────────────────────────────────────────────────────
// Variant-coverage anchor
// ─────────────────────────────────────────────────────────────────────────────

/// Read the `snake_case` wire tags declared by one `#[serde(tag = "type",
/// rename_all = "snake_case")]` enum straight out of the crate source.
///
/// This exists because `ClientCommand` / `ClientEvent` are `#[non_exhaustive]`:
/// an integration test is a DOWNSTREAM crate, so a `match` over them always
/// needs a wildcard arm and the compiler can never force a new variant to be
/// handled here. Parsing the declaration is the only mechanism left that makes
/// adding a variant automatically visible to [`every_variant_has_a_golden`] —
/// a hand-maintained count or variant list would just drift the same way the
/// goldens did.
fn declared_wire_tags(source_file: &str, enum_name: &str) -> BTreeSet<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/protocol")
        .join(source_file);
    let src = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let header = format!("pub enum {enum_name} {{");
    let start = src
        .find(&header)
        .unwrap_or_else(|| panic!("`{header}` not found in {source_file}"));

    let mut tags = BTreeSet::new();
    let mut depth = 0_i32;
    let mut renamed_tag = None;
    for line in src[start..].lines() {
        let trimmed = line.trim_start();
        // Doc comments legitimately contain unbalanced-looking braces; skip
        // them before counting, and never read a variant name out of one.
        if !trimmed.starts_with("//") {
            if depth == 1 {
                if let Some(tag) = trimmed
                    .strip_prefix("#[serde(rename = \"")
                    .and_then(|value| value.strip_suffix("\")]"))
                {
                    renamed_tag = Some(tag.to_string());
                } else {
                    assert!(
                        !(trimmed.starts_with("#[") && trimmed.contains("rename")),
                        "unsupported serde rename syntax in {enum_name}"
                    );
                }
                if let Some(name) = line
                    .strip_prefix("    ")
                    .filter(|rest| rest.starts_with(|c: char| c.is_ascii_uppercase()))
                {
                    let name: String = name
                        .chars()
                        .take_while(char::is_ascii_alphanumeric)
                        .collect();
                    tags.insert(renamed_tag.take().unwrap_or_else(|| to_snake_case(&name)));
                }
            }
            depth += i32::try_from(line.matches('{').count()).expect("brace count fits i32");
            depth -= i32::try_from(line.matches('}').count()).expect("brace count fits i32");
        }
        if depth == 0 && !tags.is_empty() {
            break;
        }
    }
    assert!(
        !tags.is_empty(),
        "no variants parsed out of {enum_name} — the extractor is broken, not the enum"
    );
    tags
}

/// serde's `rename_all = "snake_case"` rule: lowercase, `_` before each
/// non-leading uppercase letter.
fn to_snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// The wire tags actually covered by a golden table.
fn goldened_wire_tags<T: Serialize>(goldens: &[(&'static str, T)]) -> BTreeSet<String> {
    goldens
        .iter()
        .map(|(filename, value)| {
            let json = serde_json::to_value(value)
                .unwrap_or_else(|e| panic!("serialize golden instance for {filename}: {e}"));
            json.get("type")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("golden {filename} is not internally tagged on `type`"))
                .to_string()
        })
        .collect()
}

/// The permission DTOs — one golden per `PermissionKindDto` variant + the
/// resolution + the worker-bearing request.
fn permission_request_goldens() -> Vec<(&'static str, PermissionRequest)> {
    vec![
        (
            "permission/request_tool_use_confirm.json",
            PermissionRequest {
                request_id: 7,
                kind: PermissionKindDto::ToolUseConfirm {
                    tool_name: "Bash".to_string(),
                    tool_input_json: r#"{"command":"ls -la"}"#.to_string(),
                    default_allow: false,
                },
                worker: None,
                owner: None,
                suppress_always_allow_rule: false,
                auto_mode_prompt: None,
            },
        ),
        (
            "permission/request_exit_plan_mode.json",
            PermissionRequest {
                request_id: 8,
                kind: PermissionKindDto::ExitPlanMode {
                    plan: "1. read files\n2. edit".to_string(),
                },
                worker: None,
                owner: None,
                suppress_always_allow_rule: false,
                auto_mode_prompt: None,
            },
        ),
        (
            "permission/request_bypass_permissions_mode.json",
            PermissionRequest {
                request_id: 9,
                kind: PermissionKindDto::BypassPermissionsMode,
                worker: None,
                owner: None,
                suppress_always_allow_rule: false,
                auto_mode_prompt: None,
            },
        ),
        (
            "permission/request_with_worker.json",
            PermissionRequest {
                request_id: 10,
                kind: PermissionKindDto::ToolUseConfirm {
                    tool_name: "Edit".to_string(),
                    tool_input_json: r#"{"file_path":"/tmp/x"}"#.to_string(),
                    default_allow: true,
                },
                worker: Some(WorkerInfoDto {
                    name: "reviewer".to_string(),
                    color: "cyan".to_string(),
                    team: None,
                }),
                owner: None,
                suppress_always_allow_rule: false,
                auto_mode_prompt: None,
            },
        ),
    ]
}

/// The `computer` tool `request_access` DTOs — one golden per
/// `ComputerAccessRequestDto` example (`tcc_state` present / absent) plus the
/// response.
fn computer_access_goldens() -> Vec<(&'static str, ComputerAccessRequestDto)> {
    vec![
        (
            "computer_access/request_with_tcc_state.json",
            ComputerAccessRequestDto {
                request_id: 42,
                reason: "automate chat".to_string(),
                apps: vec![
                    RequestedAppDto {
                        label: "Slack".to_string(),
                    },
                    RequestedAppDto {
                        label: "Chrome".to_string(),
                    },
                ],
                tier: AccessTierDto::Full,
                clipboard_read: false,
                clipboard_write: false,
                system_key_combos: false,
                tcc_state: Some(TccStateDto {
                    accessibility: true,
                    screen_recording: false,
                }),
            },
        ),
        (
            "computer_access/request_without_tcc_state.json",
            ComputerAccessRequestDto {
                request_id: 7,
                reason: "read the clipboard".to_string(),
                apps: vec![RequestedAppDto {
                    label: "Notes".to_string(),
                }],
                tier: AccessTierDto::Read,
                clipboard_read: true,
                clipboard_write: false,
                system_key_combos: false,
                tcc_state: None,
            },
        ),
    ]
}

/// The error DTOs — one golden per `ClientError` variant.
fn error_goldens() -> Vec<(&'static str, ClientError)> {
    vec![
        (
            "error/transport.json",
            ClientError::Transport {
                message: "socket closed".to_string(),
            },
        ),
        (
            "error/protocol.json",
            ClientError::Protocol {
                message: "unknown frame tag".to_string(),
            },
        ),
        (
            "error/rejected.json",
            ClientError::Rejected {
                message: "permission denied".to_string(),
            },
        ),
        (
            "error/not_found.json",
            ClientError::NotFound {
                message: "no such session".to_string(),
            },
        ),
        (
            "error/internal.json",
            ClientError::Internal {
                message: "unexpected state".to_string(),
            },
        ),
    ]
}

// ── shared canonical sub-instances ────────────────────────────────────────────

fn canonical_cost() -> CostDto {
    CostDto {
        total_usd: 0.0123,
        input_tokens: 1200,
        output_tokens: 340,
        api_calls: 3,
        session_duration_secs: 42,
        formatted: "$0.0123".to_string(),
    }
}

/// The `MessageDto` block set — ONE block of each `MessageBlockDto` kind, in the
/// TUI scrollback render order. This is the block-set parity anchor (plan F1-02 /
/// F1-08): the golden enumerates exactly `Text | Thinking | RedactedThinking |
/// CompactBoundary | ToolUse | ToolResult`.
fn canonical_message() -> MessageDto {
    MessageDto {
        loop_wakeup: None,
        role: "assistant".to_string(),
        blocks: vec![
            MessageBlockDto::Text {
                text: "Here is the plan.".to_string(),
            },
            MessageBlockDto::Thinking {
                thinking: "I should read the file first.".to_string(),
                signature: Some("sig_think".to_string()),
            },
            MessageBlockDto::RedactedThinking {
                data: "REDACTED_BASE64".to_string(),
            },
            MessageBlockDto::CompactBoundary {
                messages_before: 12,
                messages_after: 3,
                summary: "hidden compact summary".to_string(),
            },
            MessageBlockDto::ToolUse {
                id: "toolu_01".to_string(),
                tool: "Edit".to_string(),
                input_json: r#"{"file_path":"/tmp/x","old_string":"a","new_string":"b"}"#
                    .to_string(),
                header: None,
            },
            MessageBlockDto::ToolResult {
                id: "toolu_01".to_string(),
                tool: "Edit".to_string(),
                result_json: r#"{"ok":true}"#.to_string(),
                is_error: false,
                old_string: Some("a".to_string()),
                new_string: Some("b".to_string()),
                file_path: Some("/tmp/x".to_string()),
                display: None,
            },
        ],
        images: Vec::new(),
    }
}

fn canonical_status() -> StatusSnapshotDto {
    StatusSnapshotDto {
        session_id: "11111111-1111-4111-8111-111111111111".to_string(),
        model: "claude-opus-4-7".to_string(),
        n_messages: 17,
        total_cost_usd: 0.0123,
        input_tokens: 1200,
        output_tokens: 340,
        n_mcp_connected: 1,
        n_mcp_total: 2,
        n_hooks: 1,
        n_agents: 1,
        started_at: "2026-06-02T12:00:00Z".to_string(),
        cwd: "/home/dev/project".to_string(),
        status_line: Some("opus | $0.0123".to_string()),
        active_workers: Some(2),
    }
}

fn canonical_conversation_controls() -> ConversationControlsDto {
    ConversationControlsDto {
        qualified_model: "gemini/gemini-2.5-pro".to_string(),
        permission: PermissionControlStateDto {
            requested: "auto".to_string(),
            effective: "acceptEdits".to_string(),
            options: vec![
                PermissionModeOptionDto {
                    mode: "default".to_string(),
                    available: true,
                    disabled_reason: None,
                },
                PermissionModeOptionDto {
                    mode: "acceptEdits".to_string(),
                    available: true,
                    disabled_reason: None,
                },
                PermissionModeOptionDto {
                    mode: "plan".to_string(),
                    available: true,
                    disabled_reason: None,
                },
                PermissionModeOptionDto {
                    mode: "auto".to_string(),
                    available: true,
                    disabled_reason: None,
                },
                PermissionModeOptionDto {
                    mode: "dontAsk".to_string(),
                    available: true,
                    disabled_reason: None,
                },
                PermissionModeOptionDto {
                    mode: "bypassPermissions".to_string(),
                    available: false,
                    disabled_reason: Some(ControlDisabledReasonDto {
                        code: "not_yet_available".to_string(),
                        message: Some("Bypass permissions is not available on iOS".to_string()),
                    }),
                },
            ],
        },
        reasoning: ReasoningControlStateDto {
            requested: ReasoningSelectionDto::TokenBudget { tokens: 2048 },
            effective: ReasoningSelectionDto::TokenBudget { tokens: 2048 },
            spec: ReasoningControlSpecDto {
                options: vec![
                    ReasoningOptionDto {
                        selection: ReasoningSelectionDto::Automatic,
                        persistable: true,
                    },
                    ReasoningOptionDto {
                        selection: ReasoningSelectionDto::Disabled,
                        persistable: true,
                    },
                    ReasoningOptionDto {
                        selection: ReasoningSelectionDto::Enabled,
                        persistable: false,
                    },
                    ReasoningOptionDto {
                        selection: ReasoningSelectionDto::TokenBudget { tokens: 2048 },
                        persistable: true,
                    },
                ],
                budget_range: Some(ReasoningBudgetRangeDto {
                    min_tokens: 128,
                    max_tokens: 8192,
                }),
                provider_default: ReasoningSelectionDto::Enabled,
                forced_reasoning: false,
                editable: true,
                disabled_reason: None,
            },
        },
    }
}

fn canonical_doctor() -> DoctorReportDto {
    DoctorReportDto {
        checks: vec![
            DoctorCheckDto {
                name: "config-dir".to_string(),
                status: CheckStatusDto::Pass,
                detail: None,
            },
            DoctorCheckDto {
                name: "api-key".to_string(),
                status: CheckStatusDto::Warn,
                detail: Some("using env override".to_string()),
            },
        ],
        summary: DoctorSummaryDto {
            passed: 1,
            warnings: 1,
            failed: 0,
        },
    }
}

fn canonical_task_row() -> TaskRowDto {
    TaskRowDto {
        agent_id: None,
        unread: false,
        model: None,
        effort: None,
        kind: None,
        awaiting_plan_approval: false,
        task_id: "b12345678".to_string(),
        task_type: "bash".to_string(),
        status: TaskStatusDto::Running,
        description: "run the test suite".to_string(),
        can_resume: false,
        started_at_ms: None,
        error: None,
        stage: None,
    }
}

fn canonical_coordinator_worker() -> CoordinatorWorkerDto {
    CoordinatorWorkerDto {
        agent_id: "agent:00000000-0000-0000-0000-000000000001".to_string(),
        name: "alpha".to_string(),
        agent_type: "explorer".to_string(),
        status: "working".to_string(),
    }
}

/// The canonical local-app record: a chat-born app still in draft.
fn canonical_app_record() -> AppRecordDto {
    AppRecordDto {
        id: "habits-1a2b".to_string(),
        name: "Habits".to_string(),
        brief: "A daily habit tracker".to_string(),
        git_enabled: true,
        created_at_ms: 1_750_000_000_000,
        updated_at_ms: 1_750_000_000_001,
        workflow_state: AppWorkflowStateDto::Draft,
        conversation_id: Some("55555555-5555-4555-8555-555555555555".to_string()),
        init_session_id: None,
        workspace_rel: "apps/habits-1a2b/workspace".to_string(),
        scaffolded: true,
    }
}

/// The canonical generated manifest.
///
/// This MUST stay `Some(...)` inside [`canonical_app_details`]: it is the only
/// place an `AppManifestDto` reaches a golden, and the TS mirror's
/// `validateAppManifest` guard (clients/shared/test/snapshots.test.ts) is
/// reached only through `if ('manifest' in o)`. With `manifest: None` the
/// `skip_serializing_if` drops the key and that whole guard never executes.
fn canonical_app_manifest() -> AppManifestDto {
    AppManifestDto {
        schema_version: 2,
        runtime_api_version: 2,
        app_id: "habits-1a2b".to_string(),
        name: "Habits".to_string(),
        design_revision: 4,
        collections: vec![AppDataCollectionDto {
            id: "records".to_string(),
            label: "Records".to_string(),
            fields: vec![AppDataFieldDto {
                id: "title".to_string(),
                label: "Title".to_string(),
                field_type: AppDataFieldTypeDto::Text,
                required: true,
                options: vec![],
            }],
            enabled_by_default: true,
        }],
        allowed_domains: vec!["api.example.com".to_string()],
        // One representative device capability so the golden pins the
        // manifest-context wire spelling of the new enum family.
        capabilities: vec![AppCapabilityKindDto::Camera],
        device_context: Some(DeviceContextDto {
            os: "ios".to_string(),
            form_factor: "iphone".to_string(),
        }),
        surface: Some(AppSurfaceDto::Dom),
        runtime_profile: Some(AppRuntimeProfileBindingDto {
            family: AppRuntimeProfileDto::ReactDom,
            revision: 1,
            contract_sha256: "a".repeat(64),
        }),
        dependency_snapshot: Some(AppDependencySnapshotDto {
            requested_sha256: "b".repeat(64),
            package_sha256: "c".repeat(64),
            lockfile_sha256: "d".repeat(64),
            dependency_tree_sha256: "e".repeat(64),
            sbom_sha256: "f".repeat(64),
            toolchain_key: "pnpm@11.22.0/node@24.18.1".to_string(),
            verified_profile_contract_sha256: "a".repeat(64),
        }),
    }
}

fn canonical_app_details() -> AppDetailsDto {
    AppDetailsDto {
        app: canonical_app_record(),
        manifest: Some(canonical_app_manifest()),
        runtime_profile_status: Some(
            client::protocol::local_apps::AppRuntimeProfileStatusDto::Verified,
        ),
        runtime: AppRuntimeDetailsDto {
            state: AppRuntimeStateDto::Stopped,
            mode: Some(AppRuntimeModeDto::StaticExport),
            loopback_url: None,
            suspension_reason: Some(AppRuntimeSuspensionReasonDto::Backgrounded),
            recovery_state: Some(AppRuntimeRecoveryStateDto::Pending),
            last_error: None,
        },
        checkpoints: vec![],
    }
}

fn canonical_local_app_gate() -> LocalAppGateStatusDto {
    LocalAppGateStatusDto {
        gate_id: "ui_runner".to_string(),
        label: "UI runner available".to_string(),
        status: LocalAppVerificationStatusDto::Pending,
        available: true,
        detail: Some("Will run after approval.".to_string()),
    }
}

fn canonical_local_app_tool(name: &str) -> LocalAppMcpToolSurfaceDto {
    LocalAppMcpToolSurfaceDto {
        name: name.to_string(),
        title: Some("Track habits".to_string()),
        description: Some("Create or update one habit entry.".to_string()),
        input_schema_json:
            r#"{"type":"object","properties":{"date":{"type":"string"}},"required":["date"]}"#
                .to_string(),
        output_schema_json: Some(
            r#"{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}"#
                .to_string(),
        ),
        annotations_json: Some(r#"{"readOnlyHint":false}"#.to_string()),
        execution_json: Some(r#"{"taskSupport":"optional"}"#.to_string()),
        visible_meta_json: Some(r#"{"anthropic/requiresUserInteraction":true}"#.to_string()),
        semantic_flow_json: r#"{"flowId":"local-app-save","source":"active"}"#.to_string(),
        permission_ceiling: "ask".to_string(),
    }
}

fn canonical_local_app_widget() -> McpAppWidgetDto {
    McpAppWidgetDto {
        resource_uri:
            "ui://local-app/habits-1a2b/8888888888888888888888888888888888888888888888888888888888888888/mcp-app.html"
                .to_string(),
        mime_type: "text/html;profile=mcp-app".to_string(),
        resource_sha256: "8".repeat(64),
    }
}

fn canonical_local_app_profile() -> AppRuntimeProfileOptionDto {
    AppRuntimeProfileOptionDto {
        family: AppRuntimeProfileDto::ReactDom,
        revision: 1,
        contract_sha256: "8".repeat(64),
        surface: AppSurfaceDto::Dom,
        core_packages: vec![
            AppRuntimeProfilePackageDto {
                name: "react".to_string(),
                version: "19.0.0".to_string(),
            },
            AppRuntimeProfilePackageDto {
                name: "@ionic/react".to_string(),
                version: "9.0.0".to_string(),
            },
        ],
        cache_status: "bundled".to_string(),
        download_status: "bundled".to_string(),
        available: true,
        reason: None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// EVERY `ClientEvent` variant has a byte-stable golden. This is the wire-format
/// freeze: a renamed tag / retyped field / dropped variant flips the golden.
#[test]
fn every_client_event_variant_matches_golden() {
    let mut failures = Vec::new();
    for (filename, ev) in event_goldens() {
        check_golden(filename, &ev, &mut failures);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// EVERY declared `ClientCommand` / `ClientEvent` variant appears in the golden
/// table — the exhaustiveness anchor.
///
/// `every_client_*_variant_matches_golden` only checks the rows it is handed,
/// so a variant that was never added to `command_goldens()` / `event_goldens()`
/// is silently uncovered (that is exactly how `reset_app_permissions` shipped
/// without a golden). This compares the tags DECLARED in the enum source
/// against the tags the golden tables actually serialize.
#[test]
fn every_variant_has_a_golden() {
    for (source_file, enum_name, goldened) in [
        (
            "commands.rs",
            "ClientCommand",
            goldened_wire_tags(&command_goldens()),
        ),
        (
            "events.rs",
            "ClientEvent",
            goldened_wire_tags(&event_goldens()),
        ),
    ] {
        let declared = declared_wire_tags(source_file, enum_name);
        let missing: Vec<&String> = declared.difference(&goldened).collect();
        assert!(
            missing.is_empty(),
            "{enum_name} variant(s) with no golden: {missing:?}. Add a row to \
             the golden table and re-bless with `BLESS=1 cargo test -p \
             client::protocol --test snapshot_test`."
        );
        let unknown: Vec<&String> = goldened.difference(&declared).collect();
        assert!(
            unknown.is_empty(),
            "golden table serializes {enum_name} tag(s) the enum does not \
             declare: {unknown:?}"
        );
    }
}

/// EVERY `ClientCommand` variant has a byte-stable golden.
#[test]
fn every_client_command_variant_matches_golden() {
    let mut failures = Vec::new();
    for (filename, cmd) in command_goldens() {
        check_golden(filename, &cmd, &mut failures);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// EVERY permission DTO (each `PermissionKindDto`, the worker-bearing request,
/// and `PermissionResolved`) has a byte-stable golden.
#[test]
fn every_permission_dto_matches_golden() {
    let mut failures = Vec::new();
    for (filename, req) in permission_request_goldens() {
        check_golden(filename, &req, &mut failures);
    }
    check_golden(
        "permission/resolved.json",
        &PermissionResolved {
            request_id: 7,
            response: PermissionResponseDto::AllowAlways,
        },
        &mut failures,
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// EVERY `ComputerAccessRequestDto` example (`tcc_state` present / absent) has a
/// byte-stable golden, plus a standalone `ComputerAccessResponseDto` golden.
#[test]
fn every_computer_access_dto_matches_golden() {
    let mut failures = Vec::new();
    for (filename, req) in computer_access_goldens() {
        check_golden(filename, &req, &mut failures);
    }
    check_golden(
        "computer_access/response_granted.json",
        &ComputerAccessResponseDto {
            granted_apps: vec!["Slack".to_string()],
            clipboard_read: false,
            clipboard_write: false,
            system_key_combos: false,
        },
        &mut failures,
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// EVERY `ClientError` variant has a byte-stable golden.
#[test]
fn every_client_error_variant_matches_golden() {
    let mut failures = Vec::new();
    for (filename, err) in error_goldens() {
        check_golden(filename, &err, &mut failures);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The `MessageDto` block set golden — the structural parity anchor: ONE block
/// of each `MessageBlockDto` kind (`Text | Thinking | RedactedThinking |
/// CompactBoundary | ToolUse | ToolResult`) in TUI scrollback order (plan
/// F1-02 / F1-08).
#[test]
fn message_dto_block_set_matches_golden() {
    let mut failures = Vec::new();
    check_golden(
        "message/block_set.json",
        &canonical_message(),
        &mut failures,
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));

    // Defence-in-depth: the canonical message enumerates exactly the six block
    // kinds the TUI scrollback renders, in render order — so the golden is the
    // full parity set, not a subset.
    let tags: Vec<&str> = canonical_message()
        .blocks
        .iter()
        .map(|b| match b {
            MessageBlockDto::Text { .. } => "text",
            MessageBlockDto::Thinking { .. } => "thinking",
            MessageBlockDto::RedactedThinking { .. } => "redacted_thinking",
            MessageBlockDto::CompactBoundary { .. } => "compact_boundary",
            MessageBlockDto::ToolUse { .. } => "tool_use",
            MessageBlockDto::ToolResult { .. } => "tool_result",
            _ => "unknown",
        })
        .collect();
    assert_eq!(
        tags,
        vec![
            "text",
            "thinking",
            "redacted_thinking",
            "compact_boundary",
            "tool_use",
            "tool_result"
        ],
        "MessageDto block-set golden must carry exactly the TUI scrollback block kinds"
    );
}

/// The `RenderedMessage` feed-status table golden (plan F1-08): an auditable record
/// of which `RenderedMessage` kinds are LIVE-FED vs. RESERVED / feed-deferred in
/// the foundation (governing decisions §0.7 / §0.9). This makes the §5.3 "~22
/// renderers parity" claim honest — feed-deferred entries are explicitly NOT
/// claimed as live.
#[test]
fn feed_status_table_matches_golden() {
    let table = feed_status_table();
    let mut failures = Vec::new();
    check_golden("feed_status.json", &table, &mut failures);
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));

    // Sanity: the LIVE-FED set is exactly the kinds the adapter actually emits.
    // The §0.7 "light up thinking/usage" follow-up adds ThinkingDelta + UsageUpdate
    // to the live set (event_router -> emit_thinking/emit_usage -> AdapterOutputStream).
    let live: Vec<&str> = table
        .iter()
        .filter(|e| e.status == FeedStatus::LiveFed)
        .map(|e| e.rendered_message.as_str())
        .collect();
    assert_eq!(
        live,
        vec![
            "UserText",
            "AssistantText",
            "AssistantToolUse",
            "UserToolResult",
            "CompactBoundary",
            "ThinkingDelta",
            "UsageUpdate",
            "CoordinatorStatus",
        ],
        "the LIVE-FED set must include the §0.9 coordinator-activation's CoordinatorStatus \
         (now wired via emit_coordinator_status -> AdapterOutputStream)"
    );
    // The §0.9 coordinator-activation program wires CoordinatorStatus to a live
    // source (TeamRegistry::active_worker_count flows through
    // OutputStream::emit_coordinator_status -> AdapterOutputStream). The
    // reserved-now-live flip is a feed-status change only — the DTO
    // {active_workers, team} is byte-identical, so no CLIENT_PROTOCOL_VERSION bump.
    let coordinator_status = table
        .iter()
        .find(|e| e.rendered_message == "CoordinatorStatus")
        .expect("CoordinatorStatus present in the feed-status table");
    assert_eq!(coordinator_status.status, FeedStatus::LiveFed);
    // The §0.7 follow-up is taken: ThinkingDelta + UsageUpdate are now LIVE-FED.
    let thinking_delta = table
        .iter()
        .find(|e| e.rendered_message == "ThinkingDelta")
        .expect("ThinkingDelta present in the feed-status table");
    assert_eq!(thinking_delta.status, FeedStatus::LiveFed);
    let usage_update = table
        .iter()
        .find(|e| e.rendered_message == "UsageUpdate")
        .expect("UsageUpdate present in the feed-status table");
    assert_eq!(usage_update.status, FeedStatus::LiveFed);
    // The whole-block AssistantThinking synthesized form stays RESERVED (the
    // live reasoning stream flows through ThinkingDelta, not AssistantThinking).
    let thinking = table
        .iter()
        .find(|e| e.rendered_message == "AssistantThinking")
        .expect("AssistantThinking present in the feed-status table");
    assert_eq!(thinking.status, FeedStatus::Reserved);
}

// ── feed-status table model ────────────────────────────────────────────────────

/// LIVE-FED vs. RESERVED / feed-deferred classification for the feed-status
/// golden. `serde` is `snake_case`-tagged so the golden reads as
/// `"status": "live_fed"` / `"reserved"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum FeedStatus {
    /// Wired to a live engine source in the foundation.
    LiveFed,
    /// Defined + frozen, but NOT wired to a live source (round-trip only) in the
    /// foundation (decisions §0.7 / §0.9).
    Reserved,
}

/// One row of the `RenderedMessage` feed-status table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
struct FeedStatusEntry {
    /// The `RenderedMessage` / DTO kind being classified.
    rendered_message: String,
    /// LIVE-FED or RESERVED in the foundation.
    status: FeedStatus,
    /// Human-readable note on the engine source (or why it is deferred).
    note: String,
}

fn entry(rendered_message: &str, status: FeedStatus, note: &str) -> FeedStatusEntry {
    FeedStatusEntry {
        rendered_message: rendered_message.to_string(),
        status,
        note: note.to_string(),
    }
}

/// The full feed-status table. LIVE-FED first (the adapter emits them, now incl.
/// the §0.7 follow-up's `ThinkingDelta` + `UsageUpdate` and the §0.9
/// coordinator-activation's `CoordinatorStatus`), then RESERVED / feed-deferred
/// (`AssistantThinking` whole-block form, `ExitPlanMode`, `BypassPermissionsMode`,
/// the lossy session-replay events, …).
fn feed_status_table() -> Vec<FeedStatusEntry> {
    use FeedStatus::*;
    vec![
        // ── LIVE-FED (~6) ──────────────────────────────────────────────────
        entry(
            "UserText",
            LiveFed,
            "SendPrompt user text echoed into the scrollback",
        ),
        entry(
            "AssistantText",
            LiveFed,
            "OutputStream::emit_text -> ClientEvent::TextDelta",
        ),
        entry(
            "AssistantToolUse",
            LiveFed,
            "OutputStream::emit_tool_call -> ClientEvent::ToolUseStarted",
        ),
        entry(
            "UserToolResult",
            LiveFed,
            "OutputStream::emit_tool_result -> ClientEvent::ToolUseResult",
        ),
        entry(
            "CompactBoundary",
            LiveFed,
            "OutputStream::emit_compaction_completed -> ClientEvent::CompactionCompleted",
        ),
        entry(
            "ThinkingDelta",
            LiveFed,
            "OutputStream::emit_thinking -> ClientEvent::ThinkingDelta (§0.7 follow-up: event_router emits per ThinkingDelta SSE chunk)",
        ),
        entry(
            "UsageUpdate",
            LiveFed,
            "OutputStream::emit_usage -> ClientEvent::UsageUpdate (§0.7 follow-up: event_router emits on MessageStart/MessageDelta usage)",
        ),
        entry(
            "CoordinatorStatus",
            LiveFed,
            "OutputStream::emit_coordinator_status -> ClientEvent::CoordinatorStatus (§0.9 coordinator-activation: CoordinatorStatusSink pushes TeamRegistry::active_worker_count on worker status transitions)",
        ),
        // ── RESERVED / feed-deferred ───────────────────────────────────────
        entry(
            "AssistantThinking",
            Reserved,
            "whole-block thinking synthesized form has no live source; the live stream uses ThinkingDelta (decision §0.7)",
        ),
        entry(
            "RedactedThinking",
            Reserved,
            "carried only inside a synthesized MessageDto, not streamed live",
        ),
        entry(
            "ExitPlanMode",
            Reserved,
            "PermissionGate::check never sources it in the foundation (decision §0.6)",
        ),
        entry(
            "BypassPermissionsMode",
            Reserved,
            "PermissionGate::check never sources it in the foundation (decision §0.6)",
        ),
        entry(
            "WorkerInfo",
            Reserved,
            "no wire worker identity in the foundation; always None on a live request",
        ),
        entry(
            "SessionStarted",
            Reserved,
            "lifecycle event; lossy on replay (carries only session_id, decision §0.5)",
        ),
        entry(
            "SessionEnded",
            Reserved,
            "lifecycle event; not part of the live message feed",
        ),
        entry(
            "SessionResumed",
            Reserved,
            "lifecycle event (not part of the per-turn message feed); now carries the full restored transcript (session_id + messages, oldest-first) from the live ResumeSession path rather than session_id alone",
        ),
        entry(
            "TurnStarted",
            Reserved,
            "adapter-synthesized on SendPrompt receipt; no engine source",
        ),
        entry(
            "MobileInlineImage",
            Reserved,
            "mobile inline image input deferred: needs run_turn_streaming_with_image_sources (§5.12)",
        ),
    ]
}
