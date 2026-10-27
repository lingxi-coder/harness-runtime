//! F1-09 — Structural version-diff guard.
//!
//! This enforces governing decision §0.10's STRUCTURAL versioning rule, NOT a
//! naive "file changed ⇒ bump" check:
//!
//! - A REMOVED entry, a RENAMED / RETYPED tag or field, or a NEW required field
//!   on an existing variant/record ⇒ requires a MAJOR `CLIENT_PROTOCOL_VERSION`
//!   bump (the guard fails unless the major changed).
//! - A NEW variant or a NEW optional field ⇒ additive, no bump required (the
//!   guard passes).
//!
//! The "contract index" is a flat, deterministic structural fingerprint of the
//! wire contract: one line per leaf, `"<path>": "<type>"`, where `<path>` walks
//! enum → variant → field and `<type>` is the field's wire type (so a rename of
//! a tag/field shows up as a removed key + a new key, and a retype shows up as a
//! changed value). It is checked in at `snapshots/contract_index.json` and is
//! the SAME kind of frozen, auditable artifact as the F1-08 goldens.
//!
//! ## How the index is built — and what it does NOT cover
//!
//! `current_contract_index()` is a HAND-MAINTAINED table of string literals
//! kept alongside the DTOs. It is **not exhaustive**, and nothing makes it so.
//! Read the next three paragraphs before you trust a green run here.
//!
//! **A leaf that is absent from BOTH the table and the checked-in index is
//! invisible to this guard.** [`classify`] / [`breaking_entries`] walk the keys
//! of the CHECKED-IN index and ask what became of each one; a key that was
//! never written down has nothing to become. And when table and index agree,
//! `current_contract_matches_index_or_version_bumped` returns early without
//! diffing at all. So removing, renaming or retyping an unindexed field is a
//! BREAKING wire change that ships green and unbumped. The guard's promise is
//! "no breaking change to an INDEXED leaf without a major bump" — not "no
//! breaking change".
//!
//! **This is not hypothetical.** Verified at the 8.0.0 bless: 19 of the 118
//! `pub struct`/`pub enum` types declared in `src/` carry no index rows of
//! their own — among them `PermissionOwnerDto`, `PermissionResolutionDto`,
//! `AttachmentDto`, `AppSessionRowDto` and the whole `AskUserQuestion*` family,
//! even though `ClientEvent::AskUserQuestion` is a live variant. Backfilling
//! them is a deliberately PARKED work item, not an oversight to fix in passing;
//! what is not acceptable is a header that claims coverage this table does not
//! have, because that is what stops a reviewer from checking.
//!
//! **The compile-time anchor ([`contract_index_covers_every_dto`]) forces
//! awareness only for the types and enum variants it ACTUALLY CONSTRUCTS.** It
//! is a hand-maintained list too. Constructing a value there means a removed or
//! renamed type/variant breaks THIS file's compile, so it cannot vanish
//! silently; a type absent from that list gets no such protection. When you add
//! a DTO or a variant, add it to BOTH the table and the anchor — and when you
//! rely on the anchor, check that the thing you care about is really in it.
//!
//! ## Regenerating the index
//!
//! Run with `BLESS=1` to (re)write the checked-in index from
//! `current_contract_index()`, then review the diff before committing. A bless is
//! only legitimate AFTER you have classified the change and (if breaking) bumped
//! `CLIENT_PROTOCOL_VERSION`:
//!
//! ```text
//! BLESS=1 cargo test -p client --test version_guard_test
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use client::protocol::version::CLIENT_PROTOCOL_VERSION;

// ─────────────────────────────────────────────────────────────────────────────
// Contract index model
// ─────────────────────────────────────────────────────────────────────────────

/// A flat structural fingerprint of the contract: `path -> wire type`.
///
/// `BTreeMap` so the on-disk JSON is deterministically ordered and `git diff`
/// is stable. A key is a structural path such as
/// `"ClientEvent::ToolUseStarted.input_json"`; the value is the leaf's wire type
/// (`"String"`, `"u64"`, `"Option<MessageDto>"`, an enum tag marker, …).
type ContractIndex = BTreeMap<String, String>;

/// The classification of a single index diff (decision §0.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compatibility {
    /// No structural change, OR purely additive (a new variant / optional field).
    /// Requires NO major bump.
    Compatible,
    /// A removed / renamed / retyped entry, or a required field on an existing
    /// variant/record. Requires a MAJOR bump.
    Breaking,
}

/// Every entry that makes the diff from `old` (checked-in) to `new` (current)
/// BREAKING, described one per line.
///
/// - A key present in `old` but ABSENT in `new` ⇒ removed/renamed.
/// - A key present in BOTH but with a DIFFERENT type ⇒ retyped.
/// - A new required field whose parent variant/record already exists in `old`
///   ⇒ breaking.
/// - A new optional field or variant ⇒ additive ⇒ never listed here.
///
/// This is the single source of truth for [`classify`] (which is just "is this
/// list empty"), so the guard's failure message can NAME the entries that broke
/// instead of asserting that something, somewhere, did. An exit code — or a
/// message that only says "a removed / renamed / retyped entry" — does not tell
/// the next author whether the guard actually saw the symbol they deleted.
fn breaking_entries(old: &ContractIndex, new: &ContractIndex) -> Vec<String> {
    let mut broken = Vec::new();
    for (key, old_ty) in old {
        match new.get(key) {
            None => broken.push(format!("{key}: REMOVED (was {old_ty:?})")),
            Some(new_ty) if new_ty != old_ty => {
                broken.push(format!("{key}: RETYPED {old_ty:?} -> {new_ty:?}"));
            }
            Some(_) => {}
        }
    }
    for (key, new_ty) in new {
        if old.contains_key(key)
            || new_ty.starts_with("Option<")
            || !has_existing_field_parent(old, key)
        {
            continue;
        }
        broken.push(format!("{key}: ADDED REQUIRED FIELD ({new_ty:?})"));
    }
    broken
}

/// Whether a newly added dotted field belongs to a record/variant already
/// represented by the checked-in contract. A field on a completely new
/// variant remains compatible because the variant itself is additive.
fn has_existing_field_parent(old: &ContractIndex, key: &str) -> bool {
    let Some((parent, _field)) = key.rsplit_once('.') else {
        return false;
    };
    let sibling_prefix = format!("{parent}.");
    old.contains_key(parent)
        || old
            .keys()
            .any(|existing| existing.starts_with(&sibling_prefix))
}

/// Classify the structural diff from `old` (checked-in) to `new` (current).
///
/// The overall result is `Breaking` if ANY entry is breaking, else `Compatible`.
fn classify(old: &ContractIndex, new: &ContractIndex) -> Compatibility {
    if breaking_entries(old, new).is_empty() {
        Compatibility::Compatible
    } else {
        Compatibility::Breaking
    }
}

/// Parse the `major` component of a `major.minor.patch` version string.
fn major_of(version: &str) -> u64 {
    version
        .split('.')
        .next()
        .and_then(|m| m.parse::<u64>().ok())
        .unwrap_or_else(|| panic!("version {version:?} has no numeric major component"))
}

/// Whether a BREAKING contract change is legitimately covered by a version
/// bump: the CURRENT major must exceed the major blessed alongside the
/// CHECKED-IN index — never a literal pin. A literal (`current_major > 1`)
/// is only correct until the first bump ever lands: once
/// `CLIENT_PROTOCOL_VERSION` is permanently `2.x.x` or higher, a literal `> 1`
/// is permanently `true` and a SECOND breaking change with no bump at all
/// would sail through ungated. Pulled out as its own pure function so the
/// regression has a direct unit test below, independent of disk I/O.
fn major_was_bumped_past(current_major: u64, blessed_major: u64) -> bool {
    current_major > blessed_major
}

// ─────────────────────────────────────────────────────────────────────────────
// On-disk index
// ─────────────────────────────────────────────────────────────────────────────

fn index_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("snapshots")
        .join("contract_index.json")
}

/// Sidecar recording the `CLIENT_PROTOCOL_VERSION` MAJOR that was current the
/// last time `contract_index.json` was blessed. LOAD-BEARING (finding,
/// post-Task-2 review): the breaking-change threshold must travel with the
/// version, not be a literal pin — `assert!(current_major > 1)` looks correct
/// the day it's written but is silently permanent once any bump ever lands: a
/// SECOND breaking change after `CLIENT_PROTOCOL_VERSION` is already `2.x.x`
/// would satisfy `2 > 1` with no bump at all. Comparing against the major
/// blessed alongside the CHECKED-IN index (this file), the same way the index
/// itself is a checked-in/current diff, means every future breaking change —
/// not just the first one — is gated.
fn blessed_major_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("snapshots")
        .join("blessed_major.txt")
}

/// Read the major recorded at the last bless. `None` only for a repo that
/// predates this sidecar (never happens post-bootstrap: every bless from here
/// on writes it in lockstep with `contract_index.json`).
fn read_blessed_major() -> Option<u64> {
    let raw = fs::read_to_string(blessed_major_path()).ok()?;
    Some(raw.trim().parse().unwrap_or_else(|error| {
        panic!(
            "{} does not contain a valid integer major: {error}",
            blessed_major_path().display()
        )
    }))
}

/// Write the blessed major (used under `BLESS=1`, alongside `write_index`,
/// on EVERY bless — additive or breaking — so the sidecar always reflects
/// the major of whatever contract is currently checked in.
fn write_blessed_major(major: u64) {
    let path = blessed_major_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create snapshots dir");
    }
    fs::write(&path, format!("{major}\n")).expect("write blessed_major.txt");
}

fn bless() -> bool {
    matches!(std::env::var("BLESS").as_deref(), Ok("1" | "true"))
}

/// Serialize a [`ContractIndex`] to the canonical golden string (pretty + a
/// trailing newline so the file is well-formed text and `git diff` is clean).
fn to_golden(index: &ContractIndex) -> String {
    let mut s = serde_json::to_string_pretty(index).expect("serialize contract index");
    s.push('\n');
    s
}

/// Read the checked-in index, or `None` if it does not exist yet.
fn read_checked_in_index() -> Option<ContractIndex> {
    let raw = fs::read_to_string(index_path()).ok()?;
    Some(serde_json::from_str(&raw).expect("checked-in contract_index.json is valid JSON"))
}

/// Write the index to disk (used under `BLESS=1`).
fn write_index(index: &ContractIndex) {
    let path = index_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create snapshots dir");
    }
    fs::write(&path, to_golden(index)).expect("write contract_index.json");
}

// ─────────────────────────────────────────────────────────────────────────────
// The hand-authored current contract index
// ─────────────────────────────────────────────────────────────────────────────

/// Build the current structural fingerprint of the whole contract.
///
/// This is maintained alongside the DTOs: when a DTO grows a variant/field, add
/// the matching index line(s); when a DTO drops/renames/retypes one, the change
/// here forces the guard to confirm a major bump.
///
/// Enum variants are recorded with a `<EnumName>::<Variant>` tag line (value
/// `"<variant>"`, the `snake_case` wire tag) so a renamed/removed variant is a
/// removed key; each variant field is `<EnumName>::<Variant>.<field>`. Structs
/// record `<StructName>.<field>`.
#[allow(clippy::too_many_lines)] // a flat data table: one row per contract leaf
fn current_contract_index() -> ContractIndex {
    let mut ix = ContractIndex::new();

    let mut put = |path: &str, ty: &str| {
        ix.insert(path.to_string(), ty.to_string());
    };

    put("ClientCommand::SkillAdmin", "skill_admin");
    put("ClientCommand::SkillAdmin.command", "SkillAdminCommandDto");
    put("ClientCommand::FusionAdmin", "fusion_admin");
    put(
        "ClientCommand::FusionAdmin.command",
        "FusionAdminCommandDto",
    );
    put("ClientCommand::SideChatCreate", "side_chat_create");
    put("ClientCommand::SideChatCreate.request_id", "String");
    put(
        "ClientCommand::SideChatCreate.initial_input",
        "Option<ClientTextDto>",
    );
    put("ClientCommand::SideChatList", "side_chat_list");
    put("ClientCommand::SideChatGet", "side_chat_get");
    put("ClientCommand::SideChatGet.side_id", "String");
    put("ClientCommand::SideChatEnd", "side_chat_end");
    put("ClientCommand::SideChatEnd.side_id", "String");
    put("ClientCommand::SideChatSubmit", "side_chat_submit");
    put("ClientCommand::SideChatSubmit.side_id", "String");
    put("ClientCommand::SideChatSubmit.request_id", "String");
    put("ClientCommand::SideChatSubmit.command_json", "String");
    put("FusionAdminCommandDto.action", "String");
    put("FusionAdminCommandDto.operation_id", "Option<u64>");
    put("FusionAdminCommandDto.target", "Option<String>");
    put("FusionAdminCommandDto.scope", "Option<String>");
    put("FusionAdminCommandDto.revision", "Option<String>");
    put("FusionAdminCommandDto.payload_json", "Option<String>");
    put("ConfigurationDomainDto::Fusion", "fusion");
    put("ClientEvent::FusionCatalog", "fusion_catalog");
    put("ClientEvent::FusionCatalog.catalog_json", "String");
    put(
        "ClientEvent::FusionStrategyResolved",
        "fusion_strategy_resolved",
    );
    put("ClientEvent::FusionStrategyResolved.operation_id", "u64");
    put(
        "ClientEvent::FusionStrategyResolved.selection_json",
        "Option<String>",
    );
    put(
        "ClientEvent::FusionStrategyResolved.error",
        "Option<String>",
    );
    put("ClientEvent::SideChatCreated", "side_chat_created");
    put("ClientEvent::SideChatCreated.request_id", "String");
    put("ClientEvent::SideChatCreated.side_id", "String");
    put("ClientEvent::SideChatList", "side_chat_list");
    put("ClientEvent::SideChatList.side_ids", "Vec<String>");
    put("ClientEvent::SideChatSnapshot", "side_chat_snapshot");
    put("ClientEvent::SideChatSnapshot.side_id", "String");
    put("ClientEvent::SideChatSnapshot.events_json", "Vec<String>");
    put("ClientEvent::SideChatEvent", "side_chat_event");
    put("ClientEvent::SideChatEvent.side_id", "String");
    put("ClientEvent::SideChatEvent.event_json", "String");
    put("ClientEvent::SideChatPermission", "side_chat_permission");
    put("ClientEvent::SideChatPermission.side_id", "String");
    put(
        "ClientEvent::SideChatPermission.request",
        "PermissionRequest",
    );
    put("ClientEvent::SideChatSubmitted", "side_chat_submitted");
    put("ClientEvent::SideChatSubmitted.side_id", "String");
    put("ClientEvent::SideChatSubmitted.request_id", "String");
    put("ClientEvent::SideChatEnded", "side_chat_ended");
    put("ClientEvent::SideChatEnded.side_id", "String");
    put("ClientEvent::SideChatError", "side_chat_error");
    put("ClientEvent::SideChatError.request_id", "Option<String>");
    put("ClientEvent::SideChatError.side_id", "Option<String>");
    put("ClientEvent::SideChatError.message", "String");

    // ── ClientEvent (events.rs) ───────────────────────────────────────────
    put("ClientEvent::Error", "error");
    put("ClientEvent::Error.kind", "ErrorKindDto");
    put("ClientEvent::Error.message", "ClientTextDto");
    put("ClientEvent::TerminalApiError", "terminal_api_error");
    put(
        "ClientEvent::TerminalApiError.api_error_status",
        "Option<u16>",
    );
    put("ClientEvent::TerminalApiError.terminal_reason", "String");
    put("ClientEvent::TerminalApiError.stop_reason", "String");

    put("ClientEvent::MessageIdentity", "message_identity");
    put("ClientEvent::MessageIdentity.message_id", "String");
    put("ClientEvent::MessageRetracted", "message_retracted");
    put("ClientEvent::MessageRetracted.message_id", "String");
    put("ClientEvent::ScheduledTaskFire", "scheduled_task_fire");
    put("ClientEvent::ScheduledTaskFire.message", "String");
    put("ClientEvent::SystemNotice", "system_notice");
    put("ClientEvent::SystemNotice.message", "String");
    put("ClientEvent::SystemNotice.is_error", "bool");
    put("ClientEvent::SessionStorageState", "session_storage_state");
    put("ClientEvent::SessionStorageState.disk_full", "bool");
    put("ClientEvent::UiLog", "ui_log");
    put("ClientEvent::UiLog.plugin", "String");
    put("ClientEvent::UiLog.text", "ClientTextDto");
    put("ClientEvent::UiToast", "ui_toast");
    put("ClientEvent::UiToast.plugin", "String");
    put("ClientEvent::UiToast.text", "ClientTextDto");
    put("ClientEvent::UiToast.timeout_ms", "u64");
    put("ClientEvent::UiStatus", "ui_status");
    put("ClientEvent::UiStatus.plugin", "String");
    put("ClientEvent::UiStatus.text", "Option<ClientTextDto>");
    put("ClientEvent::UiControlResult", "ui_control_result");
    put("ClientEvent::UiControlResult.request_id", "String");
    put(
        "ClientEvent::UiControlResult.response_json",
        "Option<String>",
    );
    put(
        "ClientEvent::UiControlResult.metadata_json",
        "Option<String>",
    );
    put("ClientEvent::UiControlResult.error", "Option<String>");
    put("ClientEvent::UiClientFrame", "ui_client_frame");
    put("ClientEvent::UiClientFrame.runtime_id", "String");
    put("ClientEvent::UiClientFrame.frame_json", "String");
    put("ClientEvent::PromptSuggestion", "prompt_suggestion");
    put("ClientEvent::PromptSuggestion.suggestion", "ClientTextDto");
    put("ClientEvent::PromptSuggestion.uuid", "String");
    put("ClientEvent::PromptSuggestion.session_id", "String");
    put(
        "ClientEvent::PromptSuggestionState",
        "prompt_suggestion_state",
    );
    put("ClientEvent::PromptSuggestionState.enabled", "bool");
    put(
        "ClientEvent::PromptSuggestionState.notice",
        "Option<String>",
    );
    put("ClientEvent::UiInvalidate", "ui_invalidate");
    put("ClientEvent::UiInvalidate.instances_json", "Option<String>");
    put("ClientEvent::UiInvalidate.uuid", "String");
    put("ClientEvent::UiInvalidate.session_id", "String");
    put("ClientEvent::UiNotify", "ui_notify");
    put("ClientEvent::UiNotify.plugin", "String");
    put("ClientEvent::UiNotify.text", "ClientTextDto");
    put("ClientEvent::VisualizationBlock", "visualization_block");
    put(
        "ClientEvent::VisualizationBlock.status",
        "VisualizationBlockStatusDto",
    );
    put(
        "ClientEvent::VisualizationBlock.reference",
        "Option<VisualizationRefDto>",
    );

    put("ClientEvent::TextDelta", "text_delta");
    put("ClientEvent::TextDelta.text", "ClientTextDto");

    put("ClientEvent::ToolUseStarted", "tool_use_started");
    put("ClientEvent::ToolUseStarted.id", "String");
    put("ClientEvent::ToolUseStarted.tool", "String");
    put("ClientEvent::ToolUseStarted.input_json", "String");
    put("ClientEvent::ToolUseStarted.header", "ToolHeaderDto");

    put("ClientEvent::ToolHeartbeat", "tool_heartbeat");
    put("ClientEvent::ToolHeartbeat.id", "String");
    put("ClientEvent::ToolHeartbeat.tool", "String");
    put("ClientEvent::ToolHeartbeat.elapsed_ms", "u64");

    put("ClientEvent::ToolUseResult", "tool_use_result");
    put("ClientEvent::ToolUseResult.id", "String");
    put("ClientEvent::ToolUseResult.tool", "String");
    put("ClientEvent::ToolUseResult.result_json", "String");
    put("ClientEvent::ToolUseResult.display", "ToolResultDisplayDto");
    put("ClientEvent::ToolUseResult.is_error", "bool");

    put("ClientEvent::MessageComplete", "message_complete");
    put("ClientEvent::MessageComplete.stop_reason", "Option<String>");
    put("ClientEvent::MessageComplete.message", "Option<MessageDto>");

    put("ClientEvent::TurnStarted", "turn_started");
    put("ClientEvent::TurnStarted.turn_id", "Option<u64>");

    put("ClientEvent::TurnEnded", "turn_ended");
    put("ClientEvent::TurnEnded.outcome", "TurnOutcomeDto");
    put("ClientEvent::TurnEnded.stop_reason", "Option<String>");
    put("ClientEvent::TurnEnded.cost", "CostDto");

    put("ClientEvent::TurnRecoveryState", "turn_recovery_state");
    put(
        "ClientEvent::TurnRecoveryState.snapshot",
        "TurnRecoverySnapshotDto",
    );

    put("ClientEvent::TurnEventReplay", "turn_event_replay");
    put("ClientEvent::TurnEventReplay.session_id", "String");
    put("ClientEvent::TurnEventReplay.turn_id", "u64");
    put("ClientEvent::TurnEventReplay.sequence", "u64");
    put("ClientEvent::TurnEventReplay.event_json", "String");

    put(
        "ClientEvent::TurnEventLiveUnretained",
        "turn_event_live_unretained",
    );
    put("ClientEvent::TurnEventLiveUnretained.session_id", "String");
    put("ClientEvent::TurnEventLiveUnretained.turn_id", "u64");
    put("ClientEvent::TurnEventLiveUnretained.event_json", "String");

    put("ClientEvent::TurnAttachStarted", "turn_attach_started");
    put("ClientEvent::TurnAttachStarted.request_id", "String");
    put(
        "ClientEvent::TurnAttachStarted.snapshot",
        "TurnRecoverySnapshotDto",
    );
    put("ClientEvent::TurnAttachFinished", "turn_attach_finished");
    put("ClientEvent::TurnAttachFinished.request_id", "String");
    put("ClientEvent::TurnAttachFinished.session_id", "String");
    put("ClientEvent::TurnAttachFinished.turn_id", "u64");
    put("ClientEvent::QueuedTurnStatus", "queued_turn_status");
    put("ClientEvent::QueuedTurnStatus.session_id", "String");
    put("ClientEvent::QueuedTurnStatus.turn_id", "u64");
    put("ClientEvent::QueuedTurnStatus.request_id", "String");
    put(
        "ClientEvent::QueuedTurnStatus.status",
        "QueuedTurnStatusDto",
    );
    put("QueuedTurnStatusDto::Pending", "pending");
    put("QueuedTurnStatusDto::ClaimedUncertain", "claimed_uncertain");
    put("QueuedTurnStatusDto::Delivered", "delivered");
    put("QueuedTurnStatusDto::Delivered.batch_owner_turn_id", "u64");
    put("QueuedTurnStatusDto::Delivered.member_turn_ids", "Vec<u64>");
    put("QueuedTurnStatusDto::Delivered.transcript_uuid", "String");
    put("QueuedTurnStatusDto::Cancelled", "cancelled");
    put("QueuedTurnStatusDto::Running", "running");
    put("QueuedTurnStatusDto::Terminal", "terminal");
    put(
        "QueuedTurnStatusDto::Terminal.state",
        "TurnRecoveryStateDto",
    );

    put("ClientEvent::ModSessionEvent", "mod_session_event");
    put("ClientEvent::ModSessionEvent.session_id", "String");
    put("ClientEvent::ModSessionEvent.mod_turn_id", "Option<String>");
    put(
        "ClientEvent::ModSessionEvent.payload",
        "ModSessionEventPayload",
    );
    put("ModSessionEventPayload::Notice", "notice");
    put("ModSessionEventPayload::Notice.message", "ClientTextDto");
    put("ModSessionEventPayload::Notice.is_error", "bool");
    put("ModSessionEventPayload::Notify", "notify");
    put("ModSessionEventPayload::Notify.plugin", "String");
    put("ModSessionEventPayload::Notify.text", "ClientTextDto");
    put("ModSessionEventPayload::Log", "log");
    put("ModSessionEventPayload::Log.plugin", "String");
    put("ModSessionEventPayload::Log.text", "ClientTextDto");
    put("ModSessionEventPayload::Toast", "toast");
    put("ModSessionEventPayload::Toast.plugin", "String");
    put("ModSessionEventPayload::Toast.text", "ClientTextDto");
    put("ModSessionEventPayload::Toast.timeout_ms", "u64");
    put("ModSessionEventPayload::Status", "status");
    put("ModSessionEventPayload::Status.plugin", "String");
    put(
        "ModSessionEventPayload::Status.text",
        "Option<ClientTextDto>",
    );

    put("ClientEvent::CostUpdate", "cost_update");
    put("ClientEvent::CostUpdate.total_usd", "f64");
    put("ClientEvent::CostUpdate.input_tokens", "u64");
    put("ClientEvent::CostUpdate.output_tokens", "u64");
    put("ClientEvent::CostUpdate.api_calls", "u32");
    put("ClientEvent::CostUpdate.session_duration_secs", "u64");
    put("ClientEvent::CostUpdate.formatted", "String");

    put("ClientEvent::CompactionStatus", "compaction_status");
    put("ClientEvent::CompactionStatus.phase", "String");
    put("ClientEvent::CompactionStatus.error", "Option<String>");

    put("ClientEvent::CompactionCompleted", "compaction_completed");
    put("ClientEvent::CompactionCompleted.messages_before", "u32");
    put("ClientEvent::CompactionCompleted.messages_after", "u32");
    put("ClientEvent::CompactionCompleted.bytes_saved", "u64");
    put("ClientEvent::CompactionCompleted.summary", "ClientTextDto");

    put("ClientEvent::WorkspaceChanged", "workspace_changed");
    put("ClientEvent::WorkspaceChanged.state", "WorkspaceStateDto");
    put("WorkspaceStateDto.session_id", "String");
    put("WorkspaceStateDto.cwd", "String");
    put("WorkspaceStateDto.project_root", "String");
    put("WorkspaceStateDto.additional_directories", "Vec<String>");
    put("WorkspaceStateDto.transcript_path", "String");

    put("ClientEvent::SessionStarted", "session_started");
    put("ClientEvent::SessionStarted.session_id", "String");

    put("ClientEvent::SessionEnded", "session_ended");

    put("ClientEvent::SessionResumed", "session_resumed");
    put("ClientEvent::SessionResumed.session_id", "String");
    put("ClientEvent::SessionResumed.messages", "Vec<MessageDto>");

    put("ClientEvent::SessionAgentList", "session_agent_list");
    put("ClientEvent::SessionAgentList.session_id", "String");
    put(
        "ClientEvent::SessionAgentList.agents",
        "Vec<SessionAgentSummaryDto>",
    );

    put(
        "ClientEvent::SessionAgentTranscript",
        "session_agent_transcript",
    );
    put("ClientEvent::SessionAgentTranscript.session_id", "String");
    put("ClientEvent::SessionAgentTranscript.agent_id", "String");
    put(
        "ClientEvent::SessionAgentTranscript.messages",
        "Vec<SessionAgentMessageRowDto>",
    );
    put(
        "ClientEvent::SessionAgentTranscript.next_message_index",
        "u64",
    );
    put("ClientEvent::SessionAgentTranscript.revision", "u64");

    put("ClientEvent::SessionAgentUpdated", "session_agent_updated");
    put("ClientEvent::SessionAgentUpdated.session_id", "String");
    put(
        "ClientEvent::SessionAgentUpdated.agent",
        "SessionAgentSummaryDto",
    );

    put("ClientEvent::SessionAgentMessage", "session_agent_message");
    put("ClientEvent::SessionAgentMessage.session_id", "String");
    put("ClientEvent::SessionAgentMessage.agent_id", "String");
    put("ClientEvent::SessionAgentMessage.message_index", "u64");
    put("ClientEvent::SessionAgentMessage.message_uuid", "String");
    put("ClientEvent::SessionAgentMessage.message", "MessageDto");
    put(
        "ClientEvent::SessionAgentMessage.api_error_json",
        "Option<String>",
    );
    put(
        "ClientEvent::SessionAgentTombstone",
        "session_agent_tombstone",
    );
    put("ClientEvent::SessionAgentTombstone.session_id", "String");
    put("ClientEvent::SessionAgentTombstone.agent_id", "String");
    put("ClientEvent::SessionAgentTombstone.message_uuid", "String");
    put("ClientEvent::SessionAgentTombstone.display_only", "bool");

    put("ClientEvent::SessionList", "session_list");
    put("ClientEvent::SessionList.sessions", "Vec<SessionRowDto>");

    put(
        "ClientEvent::ProviderModelCatalog",
        "provider_model_catalog",
    );
    put(
        "ClientEvent::ProviderModelCatalog.providers",
        "Vec<ProviderModelCatalogEntryDto>",
    );

    put("ClientEvent::ModelList", "model_list");
    put("ClientEvent::ModelList.models", "Vec<String>");
    put("ClientEvent::ModelList.current", "String");
    put("ClientEvent::ModelList.details", "Vec<ModelDetailsDto>");

    put("ClientEvent::ModelChanged", "model_changed");
    put("ClientEvent::ModelChanged.model", "String");

    put(
        "ClientEvent::PermissionModeChanged",
        "permission_mode_changed",
    );
    put("ClientEvent::PermissionModeChanged.mode", "String");
    put(
        "ClientEvent::ConversationControlsChanged",
        "conversation_controls_changed",
    );
    put(
        "ClientEvent::ConversationControlsChanged.controls",
        "ConversationControlsDto",
    );
    put("ClientEvent::FastModeChanged", "fast_mode_changed");
    put("ClientEvent::FastModeChanged.enabled", "bool");
    put(
        "ClientEvent::ProviderCredentialStatus",
        "provider_credential_status",
    );
    put("ClientEvent::ProviderCredentialStatus.operation_id", "u64");
    put(
        "ClientEvent::ProviderCredentialStatus.configured_provider_ids",
        "Vec<String>",
    );
    put(
        "ClientEvent::ProviderCredentialStatus.unavailable_provider_ids",
        "Vec<String>",
    );
    put(
        "ClientEvent::ProviderCredentialStatus.storage_encrypted",
        "bool",
    );
    put(
        "ClientEvent::ProviderCredentialStatus.error",
        "Option<String>",
    );
    put(
        "ClientEvent::ProviderConnectionTested",
        "provider_connection_tested",
    );
    put("ClientEvent::OpenAiOAuthUpdated", "openai_oauth_updated");
    put(
        "ClientEvent::OpenAiOAuthUpdated.session",
        "OpenAiOAuthSessionDto",
    );
    put("OpenAiOAuthSessionDto.access_token", "String");
    put("OpenAiOAuthSessionDto.refresh_token", "Option<String>");
    put("OpenAiOAuthSessionDto.expires_at", "u64");
    put("OpenAiOAuthSessionDto.account_id", "Option<String>");
    put("OpenAiOAuthSessionDto.email", "Option<String>");
    put("OpenAiOAuthSessionDto.fedramp", "bool");
    put("ClientEvent::ProviderConnectionTested.operation_id", "u64");
    put(
        "ClientEvent::ProviderConnectionTested.provider_id",
        "String",
    );
    put("ClientEvent::ProviderConnectionTested.connected", "bool");
    put("ClientEvent::ProviderConnectionTested.reachable", "bool");
    put(
        "ClientEvent::ProviderConnectionTested.authenticated",
        "bool",
    );
    put(
        "ClientEvent::ProviderConnectionTested.model_available",
        "bool",
    );
    put(
        "ClientEvent::ProviderConnectionTested.http_status",
        "Option<u16>",
    );
    put("ClientEvent::ProviderConnectionTested.latency_ms", "u64");
    put("ClientEvent::ProviderConnectionTested.message", "String");
    put(
        "ClientEvent::ProviderConnectionTested.used_stored_credential",
        "bool",
    );

    put("ClientEvent::McpServers", "mcp_servers");
    put("ClientEvent::McpServers.servers", "Vec<McpServerDto>");

    put("ClientEvent::Skills", "skills");
    put("ClientEvent::Skills.skills", "Vec<SkillDto>");
    put(
        "ClientEvent::TypescriptLspModeChanged",
        "typescript_lsp_mode_changed",
    );
    put("ClientEvent::TypescriptLspModeChanged.requested", "String");
    put("ClientEvent::TypescriptLspModeChanged.effective", "String");
    put("ClientEvent::TypescriptLspModeChanged.available", "bool");

    put("ClientEvent::Hooks", "hooks");
    put("ClientEvent::Hooks.hooks", "Vec<HookDto>");

    put("ClientEvent::Agents", "agents");
    put("ClientEvent::Agents.agents", "Vec<AgentDto>");

    put("ClientEvent::SlashCommandCatalog", "slash_command_catalog");
    put(
        "ClientEvent::SlashCommandCatalog.commands",
        "Vec<SlashCommandDto>",
    );
    put("ClientEvent::SlashCommandResult", "slash_command_result");
    put("ClientEvent::SlashCommandResult.turn_id", "Option<u64>");
    put("ClientEvent::SlashCommandResult.display", "ClientTextDto");
    put("ClientEvent::SlashCommandResult.is_error", "bool");

    put("ClientEvent::MemoryEntries", "memory_entries");
    put("ClientEvent::MemoryEntries.entries", "Vec<MemoryEntryDto>");

    put("ClientEvent::StatusSnapshot", "status_snapshot");
    put("ClientEvent::StatusSnapshot.snapshot", "StatusSnapshotDto");

    put("ClientEvent::SettingsSnapshot", "settings_snapshot");
    put("ClientEvent::SettingsSnapshot.effective_json", "String");
    put("ClientEvent::SettingsSnapshot.provenance_json", "String");
    // ADDED (additive — new optional fields need no major bump, §0.10).
    put("ClientEvent::SettingsSnapshot.files_json", "Option<String>");
    put(
        "ClientEvent::SettingsSnapshot.active_json",
        "Option<String>",
    );
    put(
        "ClientEvent::SettingsSnapshot.locked",
        "Option<Vec<String>>",
    );
    put(
        "ClientEvent::SettingsSnapshot.layers_json",
        "Option<String>",
    );
    put(
        "ClientEvent::SettingsSnapshot.merged_keys",
        "Option<Vec<String>>",
    );

    put("ClientEvent::AuthState", "auth_state");
    put("ClientEvent::AuthState.state", "AuthStateDto");

    put("ClientEvent::DoctorReport", "doctor_report");
    put("ClientEvent::DoctorReport.report", "DoctorReportDto");

    put("ClientEvent::TaskRow", "task_row");
    put("ClientEvent::TaskRow.task", "TaskRowDto");

    put("ClientEvent::TaskOutputChunk", "task_output_chunk");
    put("ClientEvent::TaskOutputChunk.task_id", "String");
    put("ClientEvent::TaskOutputChunk.content", "String");
    put("ClientEvent::TaskOutputChunk.total_lines", "u64");
    put("ClientEvent::TaskOutputChunk.truncated", "bool");

    put("ClientEvent::TaskStatusChanged", "task_status_changed");
    put("ClientEvent::TaskStatusChanged.task_id", "String");
    put("ClientEvent::TaskStatusChanged.status", "TaskStatusDto");
    put(
        "ClientEvent::TaskStatusChanged.origin_session_id",
        "Option<String>",
    );
    put("ClientEvent::TaskStatusChanged.error", "Option<String>");

    put("ClientEvent::CommandsChanged", "commands_changed");
    put(
        "ClientEvent::CommandsChanged.commands",
        "Vec<SlashCommandDto>",
    );

    put("ClientEvent::CoordinatorStatus", "coordinator_status");
    put("ClientEvent::CoordinatorStatus.active_workers", "u32");
    put("ClientEvent::CoordinatorStatus.team", "Option<String>");

    put("ClientEvent::CoordinatorWorker", "coordinator_worker");
    put(
        "ClientEvent::CoordinatorWorker.worker",
        "CoordinatorWorkerDto",
    );

    put("ClientEvent::ThinkingDelta", "thinking_delta");
    put("ClientEvent::ThinkingDelta.thinking", "String");
    put("ClientEvent::ThinkingDelta.signature", "Option<String>");

    put("ClientEvent::UsageUpdate", "usage_update");
    put("ClientEvent::UsageUpdate.is_snapshot", "Option<bool>");
    put("ClientEvent::UsageUpdate.input_tokens", "u64");
    put("ClientEvent::UsageUpdate.output_tokens", "u64");
    put("ClientEvent::UsageUpdate.cache_read_tokens", "u64");
    put("ClientEvent::UsageUpdate.cache_creation_tokens", "u64");

    put("ClientEvent::ApiRetry", "api_retry");
    put("ClientEvent::ApiRetry.message", "String");
    put("ClientEvent::ApiRetry.attempt", "u32");
    put("ClientEvent::ApiRetry.max_retries", "u32");
    put("ClientEvent::ApiRetry.delay_ms", "u64");

    put("ClientEvent::AudioRequest", "audio_request");
    put(
        "ClientEvent::AudioRequest.request",
        "AudioOperationRequestDto",
    );
    put("ClientEvent::AudioCancel", "audio_cancel");
    put("ClientEvent::AudioCancel.identity", "AudioOperationIdDto");
    put(
        "ClientEvent::AudioCapabilitiesChanged",
        "audio_capabilities_changed",
    );
    put(
        "ClientEvent::AudioCapabilitiesChanged.capabilities",
        "AudioCapabilitySnapshotDto",
    );
    put(
        "ClientEvent::UserTranscriptRowIdentity",
        "user_transcript_row_identity",
    );
    put("ClientEvent::UserTranscriptRowIdentity.row_token", "String");
    put("ClientEvent::UserTranscriptRowIdentity.uuid", "String");
    put(
        "ClientEvent::AssistantTranscriptRowUuids",
        "assistant_transcript_row_uuids",
    );
    put(
        "ClientEvent::AssistantTranscriptRowUuids.message_id",
        "String",
    );
    put(
        "ClientEvent::AssistantTranscriptRowUuids.uuids",
        "Vec<Option<String>>",
    );
    put("ClientEvent::QueryModelChange", "query_model_change");
    put("ClientEvent::QueryModelChange.to_model", "String");
    put("ClientEvent::AssistantBlockStart", "assistant_block_start");
    put("ClientEvent::AssistantBlockStart.block_key", "u64");
    put(
        "ClientEvent::AssistantBlockIdentity",
        "assistant_block_identity",
    );
    put("ClientEvent::AssistantBlockIdentity.block_key", "u64");
    put("ClientEvent::AssistantBlockIdentity.message_uuid", "String");
    put("ClientEvent::Tombstone", "tombstone");
    put(
        "ClientEvent::Tombstone.message",
        "ServerFallbackTombstoneMessageDto",
    );
    put("ClientEvent::Tombstone.display_only", "bool");
    put("ClientEvent::RefusalContinuation", "refusal_continuation");
    put(
        "ClientEvent::RefusalContinuation.phase",
        "RefusalContinuationPhaseDto",
    );
    put("ClientEvent::RefusalContinuation.salvage_text", "String");
    put(
        "ClientEvent::RefusalContinuation.join",
        "RefusalContinuationJoinDto",
    );
    put(
        "ClientEvent::RefusalContinuation.replaces_uuids",
        "Vec<String>",
    );
    put(
        "ClientEvent::RefusalContinuation.display_salvage_text",
        "bool",
    );
    put("RefusalContinuationPhaseDto::Begin", "begin");
    put("RefusalContinuationJoinDto::Exact", "exact");
    put("ServerFallbackTombstoneMessageDto.uuid", "String");
    put("ServerFallbackTombstoneMessageDto.message_type", "String");
    put(
        "ServerFallbackTombstoneMessageDto.message",
        "ServerFallbackProviderMessageDto",
    );
    put(
        "ServerFallbackTombstoneMessageDto.is_api_error_message",
        "Option<bool>",
    );
    put(
        "ServerFallbackTombstoneMessageDto.supersedes_uuids",
        "Option<Vec<String>>",
    );
    put("ServerFallbackProviderMessageDto.id", "Option<String>");
    put("ServerFallbackProviderMessageDto.model", "Option<String>");
    put(
        "ServerFallbackProviderMessageDto.stop_reason",
        "Option<String>",
    );
    put(
        "ServerFallbackProviderMessageDto.stop_details_json",
        "Option<String>",
    );
    put(
        "ServerFallbackProviderMessageDto.usage_json",
        "Option<String>",
    );
    put("ServerFallbackProviderMessageDto.content_json", "String");
    put("ServerFallbackTombstoneMessageDto.timestamp", "String");
    put(
        "ServerFallbackTombstoneMessageDto.request_id",
        "Option<String>",
    );
    put(
        "ServerFallbackTombstoneMessageDto.request_ref_json",
        "Option<String>",
    );

    put("AudioOperationIdDto.id", "String");
    put("AudioOperationIdDto.generation", "u64");
    put("AudioOperationIdDto.service_epoch", "u64");
    put("AudioOwnerDto::Session", "session");
    put("AudioOwnerDto::Session.session_id", "String");
    put("AudioOwnerDto::Ui", "ui");
    put("AudioOwnerDto::Ui.instance_id", "String");
    put("AudioOwnerDto::System", "system");
    put("AudioOwnerDto::System.instance_id", "String");
    put("AudioInitiatorDto.agent_id", "Option<String>");
    put("AudioInitiatorDto.tool_use_id", "Option<String>");
    put("AudioInitiatorDto.request_id", "Option<String>");
    put("AudioOperationRequestDto.identity", "AudioOperationIdDto");
    put("AudioOperationRequestDto.owner", "AudioOwnerDto");
    put(
        "AudioOperationRequestDto.initiator",
        "Option<AudioInitiatorDto>",
    );
    put("AudioOperationRequestDto.timeout_budget_ms", "Option<u64>");
    put("AudioOperationRequestDto.max_payload_bytes", "u64");
    put("AudioOperationRequestDto.operation", "AudioOperationDto");

    put("AudioOperationDto::Capture", "capture");
    put("AudioOperationDto::Capture.sample_rate_hz", "u32");
    put("AudioOperationDto::Capture.format", "String");
    put("AudioOperationDto::Play", "play");
    put("AudioOperationDto::Play.pcm_base64", "String");
    put("AudioOperationDto::Play.sample_rate_hz", "u32");
    put("AudioOperationDto::Transcribe", "transcribe");
    put("AudioOperationDto::Transcribe.audio_base64", "String");
    put("AudioOperationDto::Transcribe.mime_type", "String");
    put("AudioOperationDto::Transcribe.language", "Option<String>");
    put("AudioOperationDto::StartRecording", "start_recording");
    put("AudioOperationDto::StartRecording.sample_rate_hz", "u32");
    put("AudioOperationDto::StartRecording.format", "String");
    put("AudioOperationDto::StopRecording", "stop_recording");
    put("AudioOperationDto::StopRecording.handle", "String");
    put("AudioOperationDto::Listen", "listen");
    put("AudioOperationDto::Listen.language", "Option<String>");
    for variant in ["Synthesize", "Speak"] {
        let tag = if variant == "Speak" {
            "speak"
        } else {
            "synthesize"
        };
        put(&format!("AudioOperationDto::{variant}"), tag);
        put(&format!("AudioOperationDto::{variant}.text"), "String");
        put(
            &format!("AudioOperationDto::{variant}.language"),
            "Option<String>",
        );
        put(&format!("AudioOperationDto::{variant}.rate"), "Option<f32>");
        put(
            &format!("AudioOperationDto::{variant}.voice"),
            "Option<String>",
        );
    }
    put("AudioOperationDto::Status", "status");
    put("AudioOperationDto::Status.handle", "Option<String>");
    put("AudioOperationDto::EndOwner", "end_owner");

    // ── ErrorKindDto (events.rs) ──────────────────────────────────────────
    put("ErrorKindDto::Transport", "transport");
    put("ErrorKindDto::Protocol", "protocol");
    put("ErrorKindDto::Server", "server");
    put("ErrorKindDto::MaxTurns", "max_turns");
    put("ErrorKindDto::Rejected", "rejected");
    put("ErrorKindDto::Internal", "internal");

    // ── TurnOutcomeDto (events.rs) ────────────────────────────────────────
    put("TurnOutcomeDto::EndTurn", "end_turn");
    put("TurnOutcomeDto::MaxTurns", "max_turns");
    put("TurnOutcomeDto::Cancelled", "cancelled");

    // ── TurnRecoveryStateDto / TurnRecoverySnapshotDto (events.rs) ───────
    put("TurnRecoveryStateDto::Running", "running");
    put("TurnRecoveryStateDto::WaitingForUser", "waiting_for_user");
    put(
        "TurnRecoveryStateDto::PausedRecoverable",
        "paused_recoverable",
    );
    put("TurnRecoveryStateDto::Completed", "completed");
    put("TurnRecoveryStateDto::Failed", "failed");
    put("TurnRecoveryStateDto::Cancelled", "cancelled");
    put("TurnRecoverySnapshotDto.session_id", "String");
    put("TurnRecoverySnapshotDto.turn_id", "u64");
    put("TurnRecoverySnapshotDto.revision", "u64");
    put("TurnRecoverySnapshotDto.state", "TurnRecoveryStateDto");
    put("TurnRecoverySnapshotDto.first_sequence", "u64");
    put("TurnRecoverySnapshotDto.last_sequence", "u64");
    put("TurnRecoverySnapshotDto.safe_to_resume", "bool");
    put("TurnRecoverySnapshotDto.projection_uncertain", "bool");
    put("TurnRecoverySnapshotDto.reason", "Option<String>");

    // ── CostDto (events.rs) ───────────────────────────────────────────────
    put("CostDto.total_usd", "f64");
    put("CostDto.input_tokens", "u64");
    put("CostDto.output_tokens", "u64");
    put("CostDto.api_calls", "u32");
    put("CostDto.session_duration_secs", "u64");
    put("CostDto.formatted", "String");

    // ── ClientCommand (commands.rs) ───────────────────────────────────────
    put("ClientCommand::SendPrompt", "send_prompt");
    put("ClientCommand::SendPrompt.session_id", "Option<String>");
    put("ClientCommand::SendPrompt.text", "ClientTextDto");
    put(
        "ClientCommand::SendPrompt.prompt_mode",
        "Option<PromptModeDto>",
    );
    put("ClientCommand::SendPrompt.images", "Vec<ImageRefDto>");
    put("ClientCommand::SendPrompt.turn_id", "Option<u64>");
    put(
        "ClientCommand::SendPrompt.visualization_context",
        "Option<VisualizationRefDto>",
    );

    put("ClientCommand::Cancel", "cancel");
    put("ClientCommand::Cancel.session_id", "Option<String>");
    put("ClientCommand::Cancel.turn_id", "Option<u64>");

    put("ClientCommand::AttachTurn", "attach_turn");
    put("ClientCommand::AttachTurn.session_id", "String");
    put("ClientCommand::AttachTurn.turn_id", "u64");
    put("ClientCommand::AttachTurn.after_sequence", "Option<u64>");
    put("ClientCommand::AttachTurn.request_id", "String");
    put("ClientCommand::QueryQueuedTurn", "query_queued_turn");
    put("ClientCommand::QueryQueuedTurn.session_id", "String");
    put("ClientCommand::QueryQueuedTurn.turn_id", "u64");
    put("ClientCommand::QueryQueuedTurn.request_id", "String");

    put("ClientCommand::ResumeTurn", "resume_turn");
    put("ClientCommand::ResumeTurn.session_id", "String");
    put("ClientCommand::ResumeTurn.turn_id", "u64");

    put("ClientCommand::PauseTurn", "pause_turn");
    put("ClientCommand::PauseTurn.session_id", "String");
    put("ClientCommand::PauseTurn.turn_id", "u64");
    put("ClientCommand::PauseTurn.reason", "String");

    put(
        "ClientCommand::SetTypescriptLspMode",
        "set_typescript_lsp_mode",
    );
    put("ClientCommand::SetTypescriptLspMode.mode", "String");

    put("ClientCommand::ApprovePermission", "approve_permission");
    put("ClientCommand::ApprovePermission.request_id", "u64");
    put(
        "ClientCommand::ApprovePermission.response",
        "PermissionResponseDto",
    );

    put("ClientCommand::DenyPermission", "deny_permission");
    put("ClientCommand::DenyPermission.request_id", "u64");

    put(
        "ClientCommand::InitializeProactivity",
        "initialize_proactivity",
    );
    put(
        "ClientCommand::InitializeProactivity.proactivity",
        "Option<bool>",
    );
    put(
        "ClientCommand::InitializeProactivity.reinitialize",
        "Option<bool>",
    );

    put(
        "ClientCommand::ApproveComputerAccess",
        "approve_computer_access",
    );
    put("ClientCommand::ApproveComputerAccess.request_id", "u64");
    put(
        "ClientCommand::ApproveComputerAccess.response",
        "ComputerAccessResponseDto",
    );

    put("ClientCommand::DenyComputerAccess", "deny_computer_access");
    put("ClientCommand::DenyComputerAccess.request_id", "u64");

    put("ClientCommand::SetPermissionMode", "set_permission_mode");
    put("ClientCommand::SetPermissionMode.mode", "String");
    put(
        "ClientCommand::SetProactivityLevel",
        "set_proactivity_level",
    );
    put("ClientCommand::SetProactivityLevel.level", "String");
    put(
        "ClientCommand::SetProactivityLevel.offer_bypass",
        "Option<bool>",
    );

    put(
        "ClientCommand::ListProviderCredentials",
        "list_provider_credentials",
    );
    put("ClientCommand::ListProviderCredentials.operation_id", "u64");
    put(
        "ClientCommand::ListProviderCredentials.provider_ids",
        "Vec<String>",
    );
    put(
        "ClientCommand::SetProviderCredential",
        "set_provider_credential",
    );
    put("ClientCommand::SetProviderCredential.operation_id", "u64");
    put("ClientCommand::SetProviderCredential.provider_id", "String");
    put(
        "ClientCommand::SetProviderCredential.credential",
        "ProviderCredentialSecretDto",
    );
    put(
        "ClientCommand::DeleteProviderCredential",
        "delete_provider_credential",
    );
    put(
        "ClientCommand::DeleteProviderCredential.operation_id",
        "u64",
    );
    put(
        "ClientCommand::TestProviderConnection",
        "test_provider_connection",
    );
    put("ClientCommand::TestProviderConnection.operation_id", "u64");
    put(
        "ClientCommand::TestProviderConnection.provider_id",
        "String",
    );
    put("ClientCommand::TestProviderConnection.api_base", "String");
    put("ClientCommand::TestProviderConnection.model", "String");
    put(
        "ClientCommand::TestProviderConnection.credential_override",
        "Option<ProviderCredentialSecretDto>",
    );
    put(
        "ClientCommand::DeleteProviderCredential.provider_id",
        "String",
    );

    put("ProviderCredentialSecretDto.value", "String");

    put("ClientCommand::SetModel", "set_model");
    put("ClientCommand::SetModel.model", "ClientTextDto");

    put("ClientCommand::ListModels", "list_models");
    put(
        "ClientCommand::GetConversationControls",
        "get_conversation_controls",
    );
    put(
        "ClientCommand::SetReasoningSelection",
        "set_reasoning_selection",
    );
    put(
        "ClientCommand::SetReasoningSelection.selection",
        "ReasoningSelectionDto",
    );
    put("ClientCommand::SetFastMode", "set_fast_mode");
    put("ClientCommand::SetFastMode.enabled", "bool");
    put("ClientCommand::RunSlashCommand", "run_slash_command");
    put(
        "ClientCommand::RunSlashCommand.session_id",
        "Option<String>",
    );
    put("ClientCommand::RunSlashCommand.raw", "ClientTextDto");
    put("ClientCommand::RunSlashCommand.images", "Vec<ImageRefDto>");
    put("ClientCommand::RunSlashCommand.turn_id", "Option<u64>");

    put("ClientCommand::RefreshListings", "refresh_listings");
    put(
        "ClientCommand::RefreshListings.which",
        "Vec<ListingKindDto>",
    );

    put("ClientCommand::ListSessionAgents", "list_session_agents");
    put(
        "ClientCommand::LoadSessionAgentTranscript",
        "load_session_agent_transcript",
    );
    put(
        "ClientCommand::LoadSessionAgentTranscript.agent_id",
        "String",
    );

    put("ClientCommand::NewSession", "new_session");
    put("ClientCommand::NewSession.cwd", "Option<String>");
    put("ClientCommand::NewSession.model", "Option<ClientTextDto>");

    put("ClientCommand::ResumeSession", "resume_session");
    put("ClientCommand::ResumeSession.session_id", "String");
    put("ClientCommand::ResumeSession.cwd", "Option<String>");

    put("ClientCommand::ListSessions", "list_sessions");
    put("ClientCommand::ListSessions.limit", "Option<u32>");

    put("ClientCommand::Login", "login");
    put("ClientCommand::Logout", "logout");
    put("ClientCommand::ForceCompact", "force_compact");
    put("ClientCommand::ClearSession", "clear_session");

    put("ClientCommand::TaskList", "task_list");
    put("ClientCommand::TaskList.request_id", "Option<String>");
    put("ClientEvent::TaskListComplete", "task_list_complete");
    put("ClientEvent::TaskListComplete.request_id", "String");
    put("ClientEvent::TaskListComplete.active_count", "u64");
    put("ClientEvent::TaskListComplete.error", "Option<String>");
    put(
        "ClientCommand::TaskList.status_filter",
        "Option<TaskStatusDto>",
    );

    put("ClientCommand::TaskOutput", "task_output");
    put("ClientCommand::TaskOutput.task_id", "String");
    put("ClientCommand::TaskOutput.offset", "u64");

    put("ClientCommand::TaskStop", "task_stop");
    put("ClientCommand::TaskStop.task_id", "String");
    put("ClientCommand::TaskMessage", "task_message");
    put("ClientCommand::TaskMessage.task_id", "String");
    put("ClientCommand::TaskMessage.message", "String");

    put("ClientCommand::RequestExit", "request_exit");

    put("ClientCommand::UiRender", "ui_render");
    put("ClientCommand::UiRender.request_id", "String");
    put("ClientCommand::UiRender.request_json", "String");
    put("ClientCommand::UiClientModule", "ui_client_module");
    put("ClientCommand::UiClientModule.request_id", "String");
    put("ClientCommand::UiClientModule.plugin", "String");
    put("ClientCommand::UiMessage", "ui_message");
    put("ClientCommand::UiMessage.request_id", "String");
    put("ClientCommand::UiMessage.request_json", "String");
    put("ClientCommand::UiClientFault", "ui_client_fault");
    put("ClientCommand::UiClientFault.request_id", "String");
    put("ClientCommand::UiClientFault.request_json", "String");
    put("ClientCommand::UiClientPress", "ui_client_press");
    put("ClientCommand::UiClientPress.request_id", "String");
    put("ClientCommand::UiClientPress.request_json", "String");
    put("ClientCommand::UiPress", "ui_press");
    put("ClientCommand::UiPress.request_id", "String");
    put("ClientCommand::UiPress.request_json", "String");
    put("ClientCommand::UiInput", "ui_input");
    put("ClientCommand::UiInput.request_id", "String");
    put("ClientCommand::UiInput.request_json", "String");
    put("ClientCommand::UiSelect", "ui_select");
    put("ClientCommand::UiSelect.request_id", "String");
    put("ClientCommand::UiSelect.request_json", "String");
    put(
        "ClientCommand::UpdatePromptSuggestionContext",
        "update_prompt_suggestion_context",
    );
    put(
        "ClientCommand::UpdatePromptSuggestionContext.session_id",
        "String",
    );
    put(
        "ClientCommand::UpdatePromptSuggestionContext.focus",
        "String",
    );
    put(
        "ClientCommand::UpdatePromptSuggestionContext.input_empty",
        "bool",
    );
    put(
        "ClientCommand::UpdatePromptSuggestionContext.pending_permission",
        "bool",
    );
    put(
        "ClientCommand::UpdatePromptSuggestionContext.elicitation_active",
        "bool",
    );
    put(
        "ClientCommand::RecordPromptSuggestionSubmission",
        "record_prompt_suggestion_submission",
    );
    put(
        "ClientCommand::RecordPromptSuggestionSubmission.text",
        "String",
    );
    put(
        "ClientCommand::PromptSuggestionFeedback",
        "prompt_suggestion_feedback",
    );
    put(
        "ClientCommand::PromptSuggestionFeedback.session_id",
        "String",
    );
    put("ClientCommand::PromptSuggestionFeedback.uuid", "String");
    put("ClientCommand::PromptSuggestionFeedback.action", "String");
    put("ClientCommand::UiClientOperation", "ui_client_operation");
    put("ClientCommand::UiClientOperation.request_id", "String");
    put("ClientCommand::UiClientOperation.operation_json", "String");

    put("ClientCommand::UpdateSettings", "update_settings");
    put(
        "ClientCommand::UpdateSettings.destination",
        "WritableScopeDto",
    );
    put("ClientCommand::UpdateSettings.patch_json", "String");

    put(
        "ClientCommand::UpdatePermissionRules",
        "update_permission_rules",
    );
    put(
        "ClientCommand::UpdatePermissionRules.destination",
        "WritableScopeDto",
    );
    put(
        "ClientCommand::UpdatePermissionRules.behavior",
        "PermissionBehaviorDto",
    );
    put("ClientCommand::UpdatePermissionRules.add", "Vec<String>");
    put("ClientCommand::UpdatePermissionRules.remove", "Vec<String>");

    put(
        "ClientCommand::SetDefaultPermissionMode",
        "set_default_permission_mode",
    );
    put(
        "ClientCommand::SetDefaultPermissionMode.destination",
        "WritableScopeDto",
    );
    put("ClientCommand::SetDefaultPermissionMode.mode", "String");

    put(
        "ClientCommand::UpdateWorkspaceDirectories",
        "update_workspace_directories",
    );
    put(
        "ClientCommand::UpdateWorkspaceDirectories.destination",
        "WritableScopeDto",
    );
    put(
        "ClientCommand::UpdateWorkspaceDirectories.add",
        "Vec<String>",
    );
    put(
        "ClientCommand::UpdateWorkspaceDirectories.remove",
        "Vec<String>",
    );

    put("ClientCommand::UpsertMcpServer", "upsert_mcp_server");
    put("ClientCommand::UpsertMcpServer.scope", "WritableScopeDto");
    put("ClientCommand::UpsertMcpServer.name", "String");
    put("ClientCommand::UpsertMcpServer.config_json", "String");

    put("ClientCommand::RemoveMcpServer", "remove_mcp_server");
    put("ClientCommand::RemoveMcpServer.scope", "WritableScopeDto");
    put("ClientCommand::RemoveMcpServer.name", "String");

    put(
        "ClientCommand::GetAudioSessionContext",
        "get_audio_session_context",
    );
    put("ClientEvent::AudioSessionContext", "audio_session_context");
    put("ClientEvent::AudioSessionContext.session_id", "String");
    put("ClientEvent::AudioSessionContext.profile_id", "String");
    put("ClientEvent::AudioSessionContext.account_scope", "String");
    put("ClientCommand::StartRealtimeAudio", "start_realtime_audio");
    put("ClientCommand::StartRealtimeAudio.request_json", "String");
    put("ClientCommand::RealtimeAudioInput", "realtime_audio_input");
    put("ClientCommand::RealtimeAudioInput.input_json", "String");
    put("ClientCommand::StopRealtimeAudio", "stop_realtime_audio");
    put("ClientEvent::RealtimeAudioEvent", "realtime_audio_event");
    put("ClientEvent::RealtimeAudioEvent.session_id", "String");
    put("ClientEvent::RealtimeAudioEvent.event_json", "String");
    put("ClientCommand::AudioResponse", "audio_response");
    put(
        "ClientCommand::AudioResponse.identity",
        "AudioOperationIdDto",
    );
    put(
        "ClientCommand::AudioResponse.result",
        "AudioOperationResultDto",
    );
    put(
        "ClientCommand::UpdateAudioCapabilities",
        "update_audio_capabilities",
    );
    put(
        "ClientCommand::UpdateAudioCapabilities.capabilities",
        "AudioCapabilitySnapshotDto",
    );
    put(
        "ClientCommand::UpdateMediaSessionModes",
        "update_media_session_modes",
    );
    put(
        "ClientCommand::UpdateMediaSessionModes.modes",
        "Vec<MediaSessionModeDto>",
    );

    put("AudioErrorKindDto::PermissionDenied", "permission_denied");
    put("AudioErrorKindDto::Busy", "busy");
    put("AudioErrorKindDto::Cancelled", "cancelled");
    put("AudioErrorKindDto::Timeout", "timeout");
    put("AudioErrorKindDto::NoSpeech", "no_speech");
    put("AudioErrorKindDto::NotRecording", "not_recording");
    put("AudioErrorKindDto::Unavailable", "unavailable");
    put("AudioErrorKindDto::Unsupported", "unsupported");
    put("AudioErrorKindDto::ModelMissing", "model_missing");
    put("AudioErrorKindDto::VoiceMissing", "voice_missing");
    put("AudioErrorKindDto::InvalidRequest", "invalid_request");
    put("AudioErrorKindDto::SynthesisFailed", "synthesis_failed");
    put("AudioErrorKindDto::NativeFailure", "native_failure");
    put("AudioErrorKindDto::MediaTooLarge", "media_too_large");
    put("AudioErrorDto.kind", "AudioErrorKindDto");
    put("AudioErrorDto.message", "String");
    put("AudioStatusDto.recording", "bool");
    put("AudioStatusDto.playing", "bool");

    put(
        "AudioOperationResultDto::RecordingStarted",
        "recording_started",
    );
    put("AudioOperationResultDto::RecordingStarted.handle", "String");
    put("AudioOperationResultDto::Recording", "recording");
    put("AudioOperationResultDto::Recording.audio_base64", "String");
    put("AudioOperationResultDto::Recording.mime_type", "String");
    put("AudioOperationResultDto::Transcript", "transcript");
    put("AudioOperationResultDto::Transcript.text", "String");
    put(
        "AudioOperationResultDto::Transcript.language",
        "Option<String>",
    );
    put(
        "AudioOperationResultDto::Transcript.confidence",
        "Option<f32>",
    );
    put("AudioOperationResultDto::Synthesized", "synthesized");
    put("AudioOperationResultDto::Synthesized.pcm_base64", "String");
    put("AudioOperationResultDto::Synthesized.sample_rate_hz", "u32");
    put(
        "AudioOperationResultDto::PlaybackCompleted",
        "playback_completed",
    );
    put(
        "AudioOperationResultDto::PlaybackCompleted.duration_ms",
        "u64",
    );
    put("AudioOperationResultDto::Status", "status");
    put("AudioOperationResultDto::Status.status", "AudioStatusDto");
    put("AudioOperationResultDto::OwnerEnded", "owner_ended");
    put("AudioOperationResultDto::Failed", "failed");
    put("AudioOperationResultDto::Failed.error", "AudioErrorDto");

    put("AudioOperationKindDto::Capture", "capture");
    put("AudioOperationKindDto::Play", "play");
    put("AudioOperationKindDto::Transcribe", "transcribe");
    put("AudioOperationKindDto::Record", "record");
    put("AudioOperationKindDto::Listen", "listen");
    put("AudioOperationKindDto::Synthesize", "synthesize");
    put("AudioOperationKindDto::Speak", "speak");
    for (state, tag) in [
        ("Ready", "ready"),
        ("NeedsPermission", "needs_permission"),
        ("Busy", "busy"),
        ("MissingModel", "missing_model"),
        ("Unavailable", "unavailable"),
    ] {
        put(&format!("AudioReadinessStateDto::{state}"), tag);
    }
    put(
        "AudioOperationReadinessDto.operation",
        "AudioOperationKindDto",
    );
    put("AudioOperationReadinessDto.state", "AudioReadinessStateDto");
    put("AudioCapabilitySnapshotDto.service_epoch", "u64");
    put("AudioCapabilitySnapshotDto.support_revision", "u64");
    put(
        "AudioCapabilitySnapshotDto.supported_operations",
        "Vec<AudioOperationKindDto>",
    );
    put(
        "AudioCapabilitySnapshotDto.readiness",
        "Vec<AudioOperationReadinessDto>",
    );
    put("AudioCapabilitySnapshotDto.max_payload_bytes", "u64");

    // ── WritableScopeDto (commands.rs) ─────────────────────────────────────────
    put("WritableScopeDto::User", "user");
    put("WritableScopeDto::Local", "local");
    put("WritableScopeDto::Project", "project");

    // ── PromptModeDto (commands.rs) ───────────────────────────────────────
    put("PromptModeDto::Normal", "normal");
    put("PromptModeDto::Bash", "bash");
    put("PromptModeDto::Memory", "memory");
    put("PromptModeDto::Plan", "plan");

    // ── ImageRefDto (commands.rs) ─────────────────────────────────────────
    put("ImageRefDto.media_type", "String");
    put("ImageRefDto.base64", "String");

    // ── CommandResultDto (commands.rs) ────────────────────────────────────
    put("CommandResultDto.display", "String");
    put("CommandResultDto.injected", "Option<String>");

    // ── SkillAdminCommandDto (commands.rs) ────────────────────────────────
    put("SkillAdminCommandDto.action", "String");
    put("SkillAdminCommandDto.operation_id", "Option<u64>");
    put("SkillAdminCommandDto.target", "Option<ClientTextDto>");
    put("SkillAdminCommandDto.scope", "Option<String>");
    put("SkillAdminCommandDto.revision", "Option<String>");
    put("SkillAdminCommandDto.payload_json", "Option<String>");

    // ── ListingKindDto (commands.rs) ──────────────────────────────────────
    put("ListingKindDto::Sessions", "sessions");
    put("ListingKindDto::Models", "models");
    put("ListingKindDto::Mcp", "mcp");
    put("ListingKindDto::Skills", "skills");
    put("ListingKindDto::Hooks", "hooks");
    put("ListingKindDto::Agents", "agents");
    put("ListingKindDto::SlashCommands", "slash_commands");
    put("ListingKindDto::Memory", "memory");
    put("ListingKindDto::Status", "status");
    put("ListingKindDto::Settings", "settings");
    put("ListingKindDto::Auth", "auth");
    put("ListingKindDto::Doctor", "doctor");
    put("ListingKindDto::Tasks", "tasks");
    put("ListingKindDto::Coordinator", "coordinator");

    // ── WritableScopeDto (commands.rs) ──────────────────────────────
    put("WritableScopeDto::User", "user");
    put("WritableScopeDto::Project", "project");
    put("WritableScopeDto::Local", "local");

    // ── PermissionBehaviorDto (commands.rs) ───────────────────────────────
    put("PermissionBehaviorDto::Allow", "allow");
    put("PermissionBehaviorDto::Deny", "deny");
    put("PermissionBehaviorDto::Ask", "ask");

    // ── MessageDto / MessageBlockDto (message.rs) ─────────────────────────
    put("MessageDto.role", "String");
    put("MessageDto.sent_at_ms", "Option<i64>");
    put("MessageDto.loop_wakeup", "Option<LoopWakeupDto>");
    put("LoopWakeupDto.message", "String");
    put("LoopWakeupDto.companion", "Option<String>");
    put("LoopWakeupDto.streak", "u32");
    put("LoopWakeupDto.since_ms", "u64");
    put(
        "MessageDto.visualization_context",
        "Option<VisualizationContextDto>",
    );
    put("VisualizationContextDto.id", "String");
    put("VisualizationContextDto.revision", "u32");
    put("VisualizationContextDto.title", "String");
    put("VisualizationRefDto.id", "String");
    put("VisualizationRefDto.revision", "u32");
    put("VisualizationBlockStatusDto::Pending", "pending");
    put("VisualizationBlockStatusDto::Ready", "ready");
    put("VisualizationBlockStatusDto::Unavailable", "unavailable");
    put("VisualizationBlockStatusDto::Discarded", "discarded");
    put("MessageDto.blocks", "Vec<MessageBlockDto>");

    put("MessageBlockDto::Text", "text");
    put("MessageBlockDto::Text.text", "ClientTextDto");

    put("MessageBlockDto::Thinking", "thinking");
    put("MessageBlockDto::Thinking.thinking", "String");
    put("MessageBlockDto::Thinking.signature", "Option<String>");

    put("MessageBlockDto::RedactedThinking", "redacted_thinking");
    put("MessageBlockDto::RedactedThinking.data", "String");

    put("MessageBlockDto::CompactBoundary", "compact_boundary");
    put("MessageBlockDto::CompactBoundary.messages_before", "u32");
    put("MessageBlockDto::CompactBoundary.messages_after", "u32");
    put("MessageBlockDto::CompactBoundary.summary", "String");

    put("MessageBlockDto::ToolUse", "tool_use");
    put("MessageBlockDto::ToolUse.id", "String");
    put("MessageBlockDto::ToolUse.tool", "String");
    put("MessageBlockDto::ToolUse.input_json", "String");
    put("MessageBlockDto::ToolUse.header", "ToolHeaderDto");

    put("MessageBlockDto::ToolResult", "tool_result");
    put("MessageBlockDto::ToolResult.id", "String");
    put("MessageBlockDto::ToolResult.tool", "String");
    put("MessageBlockDto::ToolResult.result_json", "String");
    put("MessageBlockDto::ToolResult.is_error", "bool");
    put(
        "MessageBlockDto::ToolResult.display",
        "ToolResultDisplayDto",
    );
    put("MessageBlockDto::Visualization", "visualization");
    put(
        "MessageBlockDto::Visualization.reference",
        "Option<VisualizationRefDto>",
    );

    put("ClientEvent::PlanUpdated", "plan_updated");
    put("ClientEvent::PlanUpdated.tasks", "Vec<PlanTaskDto>");

    // ── tool_display.rs — the pre-derived render model ────────────────────
    put("ToolVerbDto::Update", "update");
    put("ToolVerbDto::Create", "create");
    put("ToolVerbDto::Read", "read");
    put("ToolVerbDto::Search", "search");
    put("ToolVerbDto::Shell", "shell");
    put("ToolVerbDto::Output", "output");
    put("ToolVerbDto::Kill", "kill");
    put("ToolVerbDto::Fetch", "fetch");
    put("ToolVerbDto::Task", "task");
    put("ToolVerbDto::Todo", "todo");
    put("ToolVerbDto::Skill", "skill");
    put("ToolVerbDto::Generic", "generic");

    put("ToolIconDto::Read", "read");
    put("ToolIconDto::Search", "search");
    put("ToolIconDto::List", "list");
    put("ToolIconDto::Edit", "edit");
    put("ToolIconDto::Terminal", "terminal");
    put("ToolIconDto::Globe", "globe");
    put("ToolIconDto::Workflow", "workflow");
    put("ToolIconDto::ListChecks", "list_checks");
    put("ToolIconDto::Sparkles", "sparkles");
    put("ToolIconDto::Plug", "plug");
    put("ToolIconDto::Output", "output");
    put("ToolIconDto::Stop", "stop");
    put("ToolIconDto::Wrench", "wrench");

    put("ToolSubLineDto.prefix", "String");
    put("ToolSubLineDto.text", "String");

    put("ToolHeaderDto.verb", "ToolVerbDto");
    put("ToolHeaderDto.icon", "Option<ToolIconDto>");
    put("ToolHeaderDto.label", "String");
    put("ToolHeaderDto.primary", "Option<String>");
    put("ToolHeaderDto.qualifier", "Option<String>");
    put("ToolHeaderDto.count", "Option<u32>");
    put("ToolHeaderDto.sub_line", "Option<ToolSubLineDto>");
    put("ToolHeaderDto.title", "String");

    put("SyntaxClassDto::Plain", "plain");
    put("SyntaxClassDto::Keyword", "keyword");
    put("SyntaxClassDto::TypeName", "type_name");
    put("SyntaxClassDto::Function", "function");
    put("SyntaxClassDto::StringLit", "string_lit");
    put("SyntaxClassDto::Number", "number");
    put("SyntaxClassDto::Comment", "comment");
    put("SyntaxClassDto::Punctuation", "punctuation");
    put("SyntaxClassDto::Operator", "operator");
    put("SyntaxClassDto::Variable", "variable");
    put("SyntaxClassDto::Constant", "constant");
    put("SyntaxClassDto::Attribute", "attribute");

    put("DiffLineKindDto::Add", "add");
    put("DiffLineKindDto::Remove", "remove");
    put("DiffLineKindDto::Context", "context");

    put("CodeSegmentDto.text", "String");
    put("CodeSegmentDto.class", "SyntaxClassDto");
    put("CodeSegmentDto.rgb", "Option<u32>");
    put("CodeSegmentDto.bold", "bool");
    put("CodeSegmentDto.italic", "bool");
    put("CodeSegmentDto.underline", "bool");
    put("CodeSegmentDto.emph", "bool");

    put("DiffRowDto.kind", "DiffLineKindDto");
    put("DiffRowDto.line_no", "u32");
    put("DiffRowDto.hunk", "u32");
    put("DiffRowDto.word_diffed", "bool");
    put("DiffRowDto.segments", "Vec<CodeSegmentDto>");

    put("StructuredDiffDto.file_path", "Option<String>");
    put("StructuredDiffDto.language", "Option<String>");
    put("StructuredDiffDto.gutter_width", "u32");
    put("StructuredDiffDto.additions", "u32");
    put("StructuredDiffDto.removals", "u32");
    put("StructuredDiffDto.truncated_rows", "u32");
    put("StructuredDiffDto.rows", "Vec<DiffRowDto>");

    put("HeadlineKindDto::Added", "added");
    put("HeadlineKindDto::Removed", "removed");
    put("HeadlineKindDto::AddedRemoved", "added_removed");
    put("HeadlineKindDto::LinesRead", "lines_read");
    put("HeadlineKindDto::LinesReadPartial", "lines_read_partial");
    put("HeadlineKindDto::FilesFound", "files_found");
    put(
        "HeadlineKindDto::FilesFoundTruncated",
        "files_found_truncated",
    );
    put("HeadlineKindDto::LinesFound", "lines_found");
    put("HeadlineKindDto::MatchesFound", "matches_found");
    put("HeadlineKindDto::Interrupted", "interrupted");
    put("HeadlineKindDto::NoContent", "no_content");
    put("HeadlineKindDto::Failed", "failed");
    put("HeadlineKindDto::Plain", "plain");

    put("ToolResultDisplayDto.headline", "Option<String>");
    put(
        "ToolResultDisplayDto.headline_kind",
        "Option<HeadlineKindDto>",
    );
    put("ToolResultDisplayDto.headline_args", "Vec<u32>");
    put("ToolResultDisplayDto.diff", "Option<StructuredDiffDto>");
    put("ToolResultDisplayDto.body", "Option<String>");
    put("ToolResultDisplayDto.body_lines", "u32");
    put("ToolResultDisplayDto.body_truncated", "bool");
    put("ToolResultDisplayDto.collapsed", "bool");

    put("PlanTaskStateDto::Pending", "pending");
    put("PlanTaskStateDto::InProgress", "in_progress");
    put("PlanTaskStateDto::Completed", "completed");

    put("PlanTaskDto.id", "Option<String>");
    put("PlanTaskDto.subject", "String");
    put("PlanTaskDto.active_form", "Option<String>");
    put("PlanTaskDto.state", "PlanTaskStateDto");

    // ── PermissionRequest / kinds (permission.rs) ─────────────────────────
    put("PermissionRequest.request_id", "u64");
    put("PermissionRequest.kind", "PermissionKindDto");
    put("PermissionRequest.worker", "Option<WorkerInfoDto>");

    put("PermissionKindDto::ToolUseConfirm", "tool_use_confirm");
    put("PermissionKindDto::ToolUseConfirm.tool_name", "String");
    put(
        "PermissionKindDto::ToolUseConfirm.tool_input_json",
        "String",
    );
    put("PermissionKindDto::ToolUseConfirm.default_allow", "bool");

    put("PermissionKindDto::ExitPlanMode", "exit_plan_mode");
    put("PermissionKindDto::ExitPlanMode.plan", "String");

    put(
        "PermissionKindDto::BypassPermissionsMode",
        "bypass_permissions_mode",
    );

    put("WorkerInfoDto.name", "String");
    put("WorkerInfoDto.color", "String");
    put("WorkerInfoDto.team", "Option<String>");

    put("PermissionResolved.request_id", "u64");
    put("PermissionResolved.response", "PermissionResponseDto");

    put("PermissionResponseDto::AllowOnce", "allow_once");
    put("PermissionResponseDto::AllowAlways", "allow_always");
    put("PermissionResponseDto::Deny", "deny");

    // ── Conversation controls (controls.rs) ──────────────────────────────
    put("ControlDisabledReasonDto.code", "String");
    put("ControlDisabledReasonDto.message", "Option<String>");

    put("ReasoningSelectionDto::Automatic", "automatic");
    put("ReasoningSelectionDto::Disabled", "disabled");
    put("ReasoningSelectionDto::Enabled", "enabled");
    put("ReasoningSelectionDto::Level", "level");
    put("ReasoningSelectionDto::Level.id", "String");
    put("ReasoningSelectionDto::TokenBudget", "token_budget");
    put("ReasoningSelectionDto::TokenBudget.tokens", "u64");

    put("ReasoningOptionDto.selection", "ReasoningSelectionDto");
    put("ReasoningOptionDto.persistable", "bool");

    put("ReasoningBudgetRangeDto.min_tokens", "u64");
    put("ReasoningBudgetRangeDto.max_tokens", "u64");

    put("ReasoningControlSpecDto.options", "Vec<ReasoningOptionDto>");
    put(
        "ReasoningControlSpecDto.budget_range",
        "Option<ReasoningBudgetRangeDto>",
    );
    put(
        "ReasoningControlSpecDto.provider_default",
        "ReasoningSelectionDto",
    );
    put("ReasoningControlSpecDto.forced_reasoning", "bool");
    put("ReasoningControlSpecDto.editable", "bool");
    put(
        "ReasoningControlSpecDto.disabled_reason",
        "Option<ControlDisabledReasonDto>",
    );

    put(
        "ReasoningControlStateDto.requested",
        "ReasoningSelectionDto",
    );
    put(
        "ReasoningControlStateDto.effective",
        "ReasoningSelectionDto",
    );
    put("ReasoningControlStateDto.spec", "ReasoningControlSpecDto");

    put("ModelBillingModeDto::PerToken", "per_token");
    put("ModelBillingModeDto::Subscription", "subscription");
    put("ModelBillingModeDto::Free", "free");
    put("ModelBillingModeDto::Unknown", "unknown");

    put("ModelPricingTierDto.context_threshold_tokens", "u64");
    put("ModelPricingTierDto.input_per_million", "Option<f64>");
    put("ModelPricingTierDto.output_per_million", "Option<f64>");
    put("ModelPricingTierDto.cache_read_per_million", "Option<f64>");
    put("ModelPricingTierDto.cache_write_per_million", "Option<f64>");
    put("ModelPricingTierDto.reasoning_per_million", "Option<f64>");

    put("ModelPricingDto.billing_mode", "ModelBillingModeDto");
    put("ModelPricingDto.input_per_million", "Option<f64>");
    put("ModelPricingDto.output_per_million", "Option<f64>");
    put("ModelPricingDto.cache_read_per_million", "Option<f64>");
    put("ModelPricingDto.cache_write_per_million", "Option<f64>");
    put("ModelPricingDto.reasoning_per_million", "Option<f64>");
    put("ModelPricingDto.tiers", "Vec<ModelPricingTierDto>");
    put("ModelPricingDto.source", "Option<String>");

    put("ModelCapabilitiesDto.streaming", "bool");
    put("ModelCapabilitiesDto.tools", "bool");
    put("ModelCapabilitiesDto.vision", "bool");
    put("ModelCapabilitiesDto.documents", "bool");
    put("ModelCapabilitiesDto.reasoning", "bool");
    put("ModelCapabilitiesDto.structured_output", "bool");

    put("ModelContextBudgetDto.policy", "String");
    put(
        "ModelContextBudgetDto.effective_input_budget",
        "Option<u64>",
    );
    put("ModelContextBudgetDto.hard_input_limit", "Option<u64>");
    put(
        "ModelContextBudgetDto.standard_price_input_tokens",
        "Option<u64>",
    );
    put(
        "ModelContextBudgetDto.recommendation_input_tokens",
        "Option<u64>",
    );
    put(
        "ModelContextBudgetDto.recommendation_source",
        "Option<String>",
    );

    put("ModelDetailsDto.reference", "String");
    put("ModelDetailsDto.provider_id", "String");
    put("ModelDetailsDto.provider_label", "String");
    put("ModelDetailsDto.display_name", "String");
    put("ModelDetailsDto.model_id", "String");
    put(
        "ModelDetailsDto.context_budget",
        "Option<ModelContextBudgetDto>",
    );
    put("ModelDetailsDto.description", "Option<String>");
    put("ModelDetailsDto.family", "Option<String>");
    put("ModelDetailsDto.status", "Option<String>");
    put("ModelDetailsDto.release_date", "Option<String>");
    put("ModelDetailsDto.last_updated", "Option<String>");
    put("ModelDetailsDto.knowledge_cutoff", "Option<String>");
    put("ModelDetailsDto.input_modalities", "Vec<String>");
    put("ModelDetailsDto.output_modalities", "Vec<String>");
    put("ModelDetailsDto.context_window_tokens", "Option<u64>");
    put("ModelDetailsDto.max_input_tokens", "Option<u64>");
    put("ModelDetailsDto.max_output_tokens", "Option<u64>");
    put("ModelDetailsDto.open_weights", "Option<bool>");
    put("ModelDetailsDto.attachments", "Option<bool>");
    put("ModelDetailsDto.temperature_control", "Option<bool>");
    put("ModelDetailsDto.pricing", "Option<ModelPricingDto>");
    put("ModelDetailsDto.capabilities", "ModelCapabilitiesDto");
    put("ModelDetailsDto.reasoning", "ReasoningControlSpecDto");
    put("ModelDetailsDto.supports_fast_mode", "bool");
    put("ModelDetailsDto.fusion_analyst_capable", "bool");

    put("ProviderModelCatalogEntryDto.provider_id", "String");
    put("ProviderModelCatalogEntryDto.provider_label", "String");
    put(
        "ProviderModelCatalogEntryDto.models",
        "Vec<ModelDetailsDto>",
    );
    put("ProviderModelCatalogEntryDto.group", "Option<String>");
    put(
        "ProviderModelCatalogEntryDto.connection_id",
        "Option<String>",
    );

    put("PermissionModeOptionDto.mode", "String");
    put("PermissionModeOptionDto.available", "bool");
    put(
        "PermissionModeOptionDto.disabled_reason",
        "Option<ControlDisabledReasonDto>",
    );

    put("PermissionControlStateDto.requested", "String");
    put("PermissionControlStateDto.effective", "String");
    put(
        "PermissionControlStateDto.options",
        "Vec<PermissionModeOptionDto>",
    );

    put("ConversationControlsDto.qualified_model", "String");
    put(
        "ConversationControlsDto.permission",
        "PermissionControlStateDto",
    );
    put(
        "ConversationControlsDto.reasoning",
        "ReasoningControlStateDto",
    );
    put(
        "ConversationControlsDto.proactivity",
        "Option<NativeProactivityControlStateDto>",
    );
    put("NativeProactivityControlStateDto.opted_in", "bool");
    put("NativeProactivityControlStateDto.selector_gate", "bool");
    put(
        "NativeProactivityControlStateDto.selector_gate_ever_on",
        "bool",
    );
    put(
        "NativeProactivityControlStateDto.attachments_enabled",
        "bool",
    );
    put("NativeProactivityControlStateDto.opt_in_available", "bool");
    put("NativeProactivityControlStateDto.level", "String");

    // ── ComputerAccessRequestDto / ComputerAccessResponseDto
    //    (computer_access.rs) ────────────────────────────────────────────
    put("ComputerAccessRequestDto.request_id", "u64");
    put("ComputerAccessRequestDto.reason", "String");
    put("ComputerAccessRequestDto.apps", "Vec<RequestedAppDto>");
    put("ComputerAccessRequestDto.tier", "AccessTierDto");
    put("ComputerAccessRequestDto.clipboard_read", "bool");
    put("ComputerAccessRequestDto.clipboard_write", "bool");
    put("ComputerAccessRequestDto.system_key_combos", "bool");
    put("ComputerAccessRequestDto.tcc_state", "Option<TccStateDto>");

    put("RequestedAppDto.label", "String");

    put("AccessTierDto::Read", "read");
    put("AccessTierDto::Click", "click");
    put("AccessTierDto::Full", "full");

    put("TccStateDto.accessibility", "bool");
    put("TccStateDto.screen_recording", "bool");

    put("ComputerAccessResponseDto.granted_apps", "Vec<String>");
    put("ComputerAccessResponseDto.clipboard_read", "bool");
    put("ComputerAccessResponseDto.clipboard_write", "bool");
    put("ComputerAccessResponseDto.system_key_combos", "bool");

    // ── ClientError (error.rs) ────────────────────────────────────────────
    put("ClientError::Transport", "transport");
    put("ClientError::Transport.message", "String");
    put("ClientError::Protocol", "protocol");
    put("ClientError::Protocol.message", "String");
    put("ClientError::Rejected", "rejected");
    put("ClientError::Rejected.message", "String");
    put("ClientError::NotFound", "not_found");
    put("ClientError::NotFound.message", "String");
    put("ClientError::Internal", "internal");
    put("ClientError::Internal.message", "String");
    put(
        "ClientError::SubmissionNotAdmitted",
        "submission_not_admitted",
    );
    put("ClientError::SubmissionNotAdmitted.message", "String");

    // ── Listing row / payload structs + enums (listings.rs) ───────────────
    put("SessionRowDto.uuid", "String");
    put("SessionRowDto.title", "String");
    put("SessionRowDto.modified_rfc3339", "String");
    put("SessionRowDto.message_count", "u32");
    put("SessionRowDto.path", "String");
    put("SessionRowDto.workspace_path", "Option<String>");

    put("McpServerDto.name", "String");
    put("McpServerDto.status", "McpStatusDto");
    put("McpServerDto.transport", "String");

    put("SkillDto.name", "String");
    put("SkillDto.source_dir", "String");

    put("McpStatusDto::Connected", "connected");
    put("McpStatusDto::Disconnected", "disconnected");
    put("McpStatusDto::Error", "error");
    put("McpStatusDto::Error.reason", "String");

    put("HookDto.name", "String");
    put("HookDto.event", "String");
    put("HookDto.matcher", "Option<String>");
    put("HookDto.timeout_ms", "u64");

    put("AgentDto.name", "String");
    put("AgentDto.description", "String");
    put("AgentDto.tools_allowed", "Vec<String>");

    put("SessionAgentSummaryDto.agent_id", "String");
    put("SessionAgentSummaryDto.name", "ClientTextDto");
    put("SessionAgentSummaryDto.agent_type", "String");
    put("SessionAgentSummaryDto.status", "String");
    put(
        "SessionAgentSummaryDto.latest_activity",
        "Option<ClientTextDto>",
    );
    put("SessionAgentSummaryDto.updated_at_ms", "Option<u64>");
    put("SessionAgentMessageRowDto.message_index", "u64");
    put("SessionAgentMessageRowDto.message_uuid", "String");
    put("SessionAgentMessageRowDto.message", "MessageDto");
    put("SessionAgentMessageRowDto.api_error_json", "Option<String>");

    put("SlashCommandDto.name", "ClientTextDto");
    put("SlashCommandDto.description", "ClientTextDto");
    put("SlashCommandDto.source", "String");
    put("SlashCommandDto.aliases", "Vec<ClientTextDto>");
    put("SlashCommandDto.argument_hint", "Option<ClientTextDto>");
    put("SlashCommandDto.argument_names", "Vec<ClientTextDto>");
    put("SlashCommandDto.menu_description", "Option<String>");
    put("SlashCommandDto.hidden", "bool");

    put("MemoryEntryDto.path", "String");
    put("MemoryEntryDto.tier", "MemoryTierDto");
    put("MemoryEntryDto.body", "String");
    put("MemoryEntryDto.age_days", "u64");
    put("MemoryEntryDto.size_bytes", "u64");

    put("MemoryTierDto::Session", "session");
    put("MemoryTierDto::Project", "project");
    put("MemoryTierDto::Team", "team");
    put("MemoryTierDto::User", "user");

    put("StatusSnapshotDto.session_id", "String");
    put("StatusSnapshotDto.model", "String");
    put("StatusSnapshotDto.n_messages", "u32");
    put("StatusSnapshotDto.total_cost_usd", "f64");
    put("StatusSnapshotDto.input_tokens", "u64");
    put("StatusSnapshotDto.output_tokens", "u64");
    put("StatusSnapshotDto.n_mcp_connected", "u32");
    put("StatusSnapshotDto.n_mcp_total", "u32");
    put("StatusSnapshotDto.n_hooks", "u32");
    put("StatusSnapshotDto.n_agents", "u32");
    put("StatusSnapshotDto.started_at", "String");
    put("StatusSnapshotDto.cwd", "String");
    put("StatusSnapshotDto.status_line", "Option<String>");
    put("StatusSnapshotDto.active_workers", "Option<u32>");

    put("AuthStateDto::SignedOut", "signed_out");
    put("AuthStateDto::SignedIn", "signed_in");
    put("AuthStateDto::SignedIn.email", "String");
    put("AuthStateDto::SignedIn.org_id", "String");

    put("DoctorReportDto.checks", "Vec<DoctorCheckDto>");
    put("DoctorReportDto.summary", "DoctorSummaryDto");

    put("DoctorCheckDto.name", "String");
    put("DoctorCheckDto.status", "CheckStatusDto");
    put("DoctorCheckDto.detail", "Option<String>");

    put("CheckStatusDto::Pass", "pass");
    put("CheckStatusDto::Warn", "warn");
    put("CheckStatusDto::Fail", "fail");

    put("DoctorSummaryDto.passed", "u32");
    put("DoctorSummaryDto.warnings", "u32");
    put("DoctorSummaryDto.failed", "u32");

    put("TaskRowDto.task_id", "String");
    put("TaskRowDto.task_type", "String");
    put("TaskRowDto.status", "TaskStatusDto");
    put("TaskRowDto.description", "String");
    put("TaskRowDto.can_resume", "bool");
    put("TaskRowDto.started_at_ms", "Option<u64>");
    put("TaskRowDto.error", "Option<String>");
    put("TaskRowDto.stage", "Option<String>");
    put("TaskRowDto.awaiting_plan_approval", "bool");

    put("TaskStatusDto::Pending", "pending");
    put("TaskStatusDto::Running", "running");
    put("TaskStatusDto::Paused", "paused");
    put("TaskStatusDto::Completed", "completed");
    put("TaskStatusDto::Failed", "failed");
    put("TaskStatusDto::Cancelled", "cancelled");

    put("CoordinatorWorkerDto.agent_id", "String");
    put("CoordinatorWorkerDto.name", "String");
    put("CoordinatorWorkerDto.agent_type", "String");
    put("CoordinatorWorkerDto.status", "String");

    // ── media.rs — canonical app media and host-private bridge contracts ──
    put(
        "ClientCommand::MediaConnectionResponse",
        "media_connection_response",
    );
    put(
        "ClientCommand::MediaConnectionResponse.response",
        "MediaConnectionResponseDto",
    );
    put(
        "ClientCommand::MediaDeviceResponse",
        "media_device_response",
    );
    put(
        "ClientCommand::MediaDeviceResponse.response",
        "MediaDeviceResponseDto",
    );
    put(
        "ClientCommand::MediaServiceRequest",
        "media_service_request",
    );
    put(
        "ClientCommand::MediaServiceRequest.request",
        "MediaServiceRequestDto",
    );
    put(
        "ClientCommand::MediaControlResponse",
        "media_control_response",
    );
    put(
        "ClientCommand::MediaControlResponse.response",
        "MediaControlResponseDto",
    );
    put(
        "ClientEvent::MediaConnectionRequest",
        "media_connection_request",
    );
    put(
        "ClientEvent::MediaConnectionRequest.request",
        "MediaConnectionRequestDto",
    );
    put("ClientEvent::MediaDeviceRequest", "media_device_request");
    put(
        "ClientEvent::MediaDeviceRequest.request",
        "MediaDeviceRequestEnvelopeDto",
    );
    put(
        "ClientEvent::MediaServiceResponse",
        "media_service_response",
    );
    put(
        "ClientEvent::MediaServiceResponse.response",
        "MediaServiceResponseDto",
    );
    put("ClientEvent::MediaControlRequest", "media_control_request");
    put(
        "ClientEvent::MediaControlRequest.request",
        "MediaControlRequestDto",
    );

    put("MediaOperationIdDto.id", "String");
    put("MediaOperationIdDto.generation", "u64");
    put("MediaOperationIdDto.service_epoch", "u64");
    put("MediaOwnerDto::Session", "session");
    put("MediaOwnerDto::Session.session_id", "String");
    put("MediaOwnerDto::Ui", "ui");
    put("MediaOwnerDto::Ui.instance_id", "String");
    put("MediaOwnerDto::System", "system");
    put("MediaOwnerDto::System.instance_id", "String");
    put("MediaInitiatorDto.agent_id", "Option<String>");
    put("MediaInitiatorDto.tool_use_id", "Option<String>");
    put("MediaInitiatorDto.request_id", "Option<String>");
    for (variant, tag) in [("Audio", "audio"), ("Image", "image"), ("Video", "video")] {
        put(&format!("MediaKindDto::{variant}"), tag);
    }
    for (variant, tag) in [
        ("Microphone", "microphone"),
        ("Camera", "camera"),
        ("Screen", "screen"),
        ("Speaker", "speaker"),
        ("Library", "library"),
        ("Remote", "remote"),
    ] {
        put(&format!("MediaSourceKindDto::{variant}"), tag);
    }
    for (variant, tag) in [
        ("Capture", "capture"),
        ("Record", "record"),
        ("Transcribe", "transcribe"),
        ("Play", "play"),
        ("Speak", "speak"),
        ("Session", "session"),
        ("ReadImage", "read_image"),
    ] {
        put(&format!("MediaSourceUsageDto::{variant}"), tag);
    }
    for (variant, tag) in [
        ("Sources", "sources"),
        ("Status", "status"),
        ("Capture", "capture"),
        ("Record", "record"),
        ("Transcribe", "transcribe"),
        ("Synthesize", "synthesize"),
        ("Play", "play"),
        ("Speak", "speak"),
        ("SessionStart", "session_start"),
        ("SessionUpdate", "session_update"),
        ("Observe", "observe"),
        ("Wait", "wait"),
        ("Stop", "stop"),
    ] {
        put(&format!("MediaOperationKindDto::{variant}"), tag);
    }
    for (variant, tag) in [
        ("Ready", "ready"),
        ("NeedsPermission", "needs_permission"),
        ("Busy", "busy"),
        ("MissingModel", "missing_model"),
        ("Unavailable", "unavailable"),
    ] {
        put(&format!("MediaReadinessStateDto::{variant}"), tag);
    }
    put(
        "MediaOperationReadinessDto.operation",
        "MediaOperationKindDto",
    );
    put("MediaOperationReadinessDto.state", "MediaReadinessStateDto");
    put("MediaCapabilitiesDto.service_epoch", "u64");
    put("MediaCapabilitiesDto.support_revision", "u64");
    put(
        "MediaCapabilitiesDto.supported_operations",
        "Vec<MediaOperationKindDto>",
    );
    put(
        "MediaCapabilitiesDto.supported_media_kinds",
        "Vec<MediaKindDto>",
    );
    put(
        "MediaCapabilitiesDto.readiness",
        "Vec<MediaOperationReadinessDto>",
    );
    put("MediaCapabilitiesDto.max_payload_bytes", "u64");
    put("MediaSourceRefDto.id", "String");
    put("MediaSessionRefDto.id", "String");
    put("MediaResourceRefDto::Source", "source");
    put("MediaResourceRefDto::Source.source", "MediaSourceRefDto");
    put("MediaResourceRefDto::Recording", "recording");
    put("MediaResourceRefDto::Recording.recording_id", "String");
    put("MediaResourceRefDto::Session", "session");
    put("MediaResourceRefDto::Session.session_id", "String");
    put("MediaResourceRefDto::Media", "media");
    put("MediaResourceRefDto::Media.media_ref", "String");
    put("MediaResourceRefDto::Operation", "operation");
    put(
        "MediaResourceRefDto::Operation.operation_id",
        "MediaOperationIdDto",
    );
    put("MediaSourceDto.reference", "MediaSourceRefDto");
    put("MediaSourceDto.kind", "MediaSourceKindDto");
    put("MediaSourceDto.media_kind", "Option<MediaKindDto>");
    put("MediaSourceDto.label", "String");
    put("MediaSourceDto.usages", "Vec<MediaSourceUsageDto>");
    put(
        "MediaSourceDto.supported_operations",
        "Vec<MediaOperationKindDto>",
    );
    put("MediaSourceDto.is_default", "bool");
    put("MediaMetadataDto.kind", "MediaKindDto");
    put("MediaMetadataDto.mime_type", "String");
    put("MediaMetadataDto.byte_len", "u64");
    put("MediaMetadataDto.captured_at_unix_ms", "Option<u64>");
    put("MediaMetadataDto.duration_ms", "Option<u64>");
    put("MediaMetadataDto.width_px", "Option<u32>");
    put("MediaMetadataDto.height_px", "Option<u32>");
    put("MediaMetadataDto.sample_rate_hz", "Option<u32>");
    put("MediaMetadataDto.channels", "Option<u16>");
    put("MediaAssetDto.media_ref", "String");
    put("MediaAssetDto.metadata", "MediaMetadataDto");
    put("MediaCaptureOptionsDto.max_duration_ms", "Option<u64>");
    put("MediaCaptureOptionsDto.max_dimension_px", "Option<u32>");
    put("MediaCaptureOptionsDto.jpeg_quality", "Option<f32>");
    put("MediaRecordOptionsDto.max_duration_ms", "Option<u64>");
    put("MediaRecordOptionsDto.mime_type", "Option<String>");
    put("MediaTranscriptionInputDto::LiveSource", "live_source");
    put(
        "MediaTranscriptionInputDto::LiveSource.source",
        "MediaSourceRefDto",
    );
    put("MediaTranscriptionInputDto::StoredMedia", "stored_media");
    put(
        "MediaTranscriptionInputDto::StoredMedia.media_ref",
        "String",
    );
    put("MediaSessionModeDto::Audio", "audio");
    put("MediaSessionModeDto::Video", "video");
    put("MediaSessionModeDto::AudioVideo", "audio_video");
    put("MediaSessionIntentDto::Observation", "observation");
    put("MediaSessionIntentDto::Conversation", "conversation");
    put("MediaSessionRequestDto.mode", "MediaSessionModeDto");
    put("MediaSessionRequestDto.intent", "MediaSessionIntentDto");
    put(
        "MediaSessionRequestDto.audio_source",
        "Option<MediaSourceRefDto>",
    );
    put(
        "MediaSessionRequestDto.video_source",
        "Option<MediaSourceRefDto>",
    );
    put("MediaSessionUpdateDto::SetEnabled", "set_enabled");
    put("MediaSessionUpdateDto::SetEnabled.kind", "MediaKindDto");
    put("MediaSessionUpdateDto::SetEnabled.enabled", "bool");
    put("MediaSessionUpdateDto::SelectSource", "select_source");
    put("MediaSessionUpdateDto::SelectSource.kind", "MediaKindDto");
    put(
        "MediaSessionUpdateDto::SelectSource.source",
        "MediaSourceRefDto",
    );
    put(
        "MediaSessionUpdateDto::SetPlaybackGain",
        "set_playback_gain",
    );
    put("MediaSessionUpdateDto::SetPlaybackGain.gain", "f32");
    put("MediaSessionStateDto::Preparing", "preparing");
    put("MediaSessionStateDto::Ready", "ready");
    put("MediaSessionStateDto::Reconnecting", "reconnecting");
    put("MediaSessionStateDto::Ended", "ended");
    put("MediaSessionStateDto::Failed", "failed");
    put("MediaSessionStatusDto.state", "MediaSessionStateDto");
    put("MediaSessionStatusDto.audio_enabled", "bool");
    put("MediaSessionStatusDto.video_enabled", "bool");
    put("MediaOperationRequestDto.identity", "MediaOperationIdDto");
    put("MediaOperationRequestDto.owner", "MediaOwnerDto");
    put(
        "MediaOperationRequestDto.initiator",
        "Option<MediaInitiatorDto>",
    );
    put("MediaOperationRequestDto.timeout_budget_ms", "Option<u64>");
    put("MediaOperationRequestDto.max_payload_bytes", "u64");
    put("MediaOperationRequestDto.operation", "MediaOperationDto");
    put("MediaOperationDto::Sources", "sources");
    put("MediaOperationDto::Sources.kind", "Option<MediaKindDto>");
    put(
        "MediaOperationDto::Sources.usage",
        "Option<MediaSourceUsageDto>",
    );
    put("MediaOperationDto::Status", "status");
    put(
        "MediaOperationDto::Status.target",
        "Option<MediaResourceRefDto>",
    );
    put("MediaOperationDto::Capture", "capture");
    put("MediaOperationDto::Capture.source", "MediaSourceRefDto");
    put(
        "MediaOperationDto::Capture.options",
        "MediaCaptureOptionsDto",
    );
    put("MediaOperationDto::Record", "record");
    put("MediaOperationDto::Record.source", "MediaSourceRefDto");
    put("MediaOperationDto::Record.options", "MediaRecordOptionsDto");
    put("MediaOperationDto::Transcribe", "transcribe");
    put(
        "MediaOperationDto::Transcribe.input",
        "MediaTranscriptionInputDto",
    );
    put("MediaOperationDto::Transcribe.language", "Option<String>");
    for variant in ["Synthesize", "Speak"] {
        let tag = if variant == "Speak" {
            "speak"
        } else {
            "synthesize"
        };
        put(&format!("MediaOperationDto::{variant}"), tag);
        put(&format!("MediaOperationDto::{variant}.text"), "String");
        put(
            &format!("MediaOperationDto::{variant}.language"),
            "Option<String>",
        );
        put(&format!("MediaOperationDto::{variant}.rate"), "Option<f32>");
        put(
            &format!("MediaOperationDto::{variant}.voice"),
            "Option<String>",
        );
    }
    put("MediaOperationDto::Play", "play");
    put("MediaOperationDto::Play.media_ref", "String");
    put(
        "MediaOperationDto::Play.source",
        "Option<MediaSourceRefDto>",
    );
    put("MediaOperationDto::SessionStart", "session_start");
    put(
        "MediaOperationDto::SessionStart.request",
        "MediaSessionRequestDto",
    );
    put("MediaOperationDto::SessionUpdate", "session_update");
    put("MediaOperationDto::SessionUpdate.session_id", "String");
    put(
        "MediaOperationDto::SessionUpdate.update",
        "MediaSessionUpdateDto",
    );
    put("MediaOperationDto::Observe", "observe");
    put("MediaOperationDto::Observe.target", "MediaResourceRefDto");
    put("MediaOperationDto::Observe.after_sequence", "Option<u64>");
    put("MediaOperationDto::Observe.limit", "u16");
    put("MediaOperationDto::Wait", "wait");
    put("MediaOperationDto::Wait.target", "MediaResourceRefDto");
    put("MediaOperationDto::Wait.after_sequence", "Option<u64>");
    put("MediaOperationDto::Wait.timeout_ms", "u64");
    put("MediaOperationDto::Wait.limit", "u16");
    put("MediaOperationDto::Stop", "stop");
    put("MediaOperationDto::Stop.target", "MediaResourceRefDto");
    put("MediaResultDto::Sources", "sources");
    put("MediaResultDto::Sources.sources", "Vec<MediaSourceDto>");
    put("MediaResultDto::Status", "status");
    put(
        "MediaResultDto::Status.resources",
        "Vec<MediaResourceStatusDto>",
    );
    put("MediaResultDto::Captured", "captured");
    put("MediaResultDto::Captured.asset", "MediaAssetDto");
    put("MediaResultDto::RecordingStarted", "recording_started");
    put("MediaResultDto::RecordingStarted.recording_id", "String");
    put("MediaResultDto::RecordingStopped", "recording_stopped");
    put("MediaResultDto::RecordingStopped.recording_id", "String");
    put("MediaResultDto::RecordingStopped.asset", "MediaAssetDto");
    put("MediaResultDto::Transcript", "transcript");
    put("MediaResultDto::Transcript.text", "String");
    put("MediaResultDto::Transcript.language", "Option<String>");
    put("MediaResultDto::Transcript.confidence", "Option<f32>");
    put("MediaResultDto::Synthesized", "synthesized");
    put("MediaResultDto::Synthesized.asset", "MediaAssetDto");
    put("MediaResultDto::PlaybackCompleted", "playback_completed");
    put("MediaResultDto::PlaybackCompleted.duration_ms", "u64");
    put("MediaResultDto::SpeechCompleted", "speech_completed");
    put("MediaResultDto::SpeechCompleted.duration_ms", "u64");
    put("MediaResultDto::SessionStarted", "session_started");
    put("MediaResultDto::SessionStarted.session_id", "String");
    put(
        "MediaResultDto::SessionStarted.status",
        "MediaSessionStatusDto",
    );
    put("MediaResultDto::SessionUpdated", "session_updated");
    put("MediaResultDto::SessionUpdated.session_id", "String");
    put(
        "MediaResultDto::SessionUpdated.status",
        "MediaSessionStatusDto",
    );
    put("MediaResultDto::Events", "events");
    put("MediaResultDto::Events.batch", "MediaEventBatchDto");
    put("MediaResultDto::Stopped", "stopped");
    put("MediaResultDto::Stopped.resource", "MediaResourceRefDto");
    put("MediaServiceActionDto::Execute", "execute");
    put(
        "MediaServiceActionDto::Execute.operation",
        "MediaOperationDto",
    );
    put(
        "MediaServiceActionDto::Execute.timeout_budget_ms",
        "Option<u64>",
    );
    put(
        "MediaServiceActionDto::Execute.max_payload_bytes",
        "Option<u64>",
    );
    put("MediaServiceActionDto::ReadMedia", "read_media");
    put("MediaServiceActionDto::ReadMedia.media_ref", "String");
    put("MediaServiceActionDto::Cancel", "cancel");
    put(
        "MediaServiceActionDto::Cancel.operation_id",
        "MediaOperationIdDto",
    );
    put(
        "MediaServiceActionDto::UpdateAudioSettings",
        "update_audio_settings",
    );
    put(
        "MediaServiceActionDto::UpdateAudioSettings.configuration_json",
        "String",
    );
    put(
        "MediaServiceActionDto::RegisterConnection",
        "register_connection",
    );
    put(
        "MediaServiceActionDto::RegisterConnection.connection_id",
        "String",
    );
    put("MediaServiceActionDto::RegisterConnection.label", "String");
    put(
        "MediaServiceActionDto::RegisterConnection.supported_modes",
        "Vec<MediaSessionModeDto>",
    );
    put(
        "MediaServiceActionDto::RegisterConnection.source_kinds",
        "Vec<MediaSourceKindDto>",
    );
    put(
        "MediaServiceActionDto::UnregisterConnection",
        "unregister_connection",
    );
    put(
        "MediaServiceActionDto::UnregisterConnection.connection_id",
        "String",
    );
    put("MediaServiceRequestDto.request_id", "String");
    put("MediaServiceRequestDto.action", "MediaServiceActionDto");
    put("MediaServiceResultDto::Operation", "operation");
    put(
        "MediaServiceResultDto::Operation.operation_id",
        "MediaOperationIdDto",
    );
    put("MediaServiceResultDto::Operation.result", "MediaResultDto");
    put("MediaServiceResultDto::Content", "content");
    put("MediaServiceResultDto::Content.content", "MediaContentDto");
    put("MediaServiceResultDto::Cancelled", "cancelled");
    put("MediaServiceResultDto::SettingsUpdated", "settings_updated");
    put(
        "MediaServiceResultDto::ConnectionUpdated",
        "connection_updated",
    );
    put("MediaServiceResponseDto.request_id", "String");
    put(
        "MediaServiceResponseDto.result",
        "Option<MediaServiceResultDto>",
    );
    put("MediaServiceResponseDto.error", "Option<MediaErrorDto>");
    put("MediaResourceStatusDto.resource", "MediaResourceRefDto");
    put("MediaResourceStatusDto.state", "MediaResourceStateDto");
    put("MediaResourceStatusDto.started_at_unix_ms", "Option<u64>");
    put("MediaResourceStatusDto.updated_at_unix_ms", "Option<u64>");
    for (variant, tag) in [
        ("Ready", "ready"),
        ("Active", "active"),
        ("Stopping", "stopping"),
        ("Completed", "completed"),
        ("Failed", "failed"),
    ] {
        put(&format!("MediaResourceStateDto::{variant}"), tag);
    }
    put("MediaEventBatchDto.target", "MediaResourceRefDto");
    put("MediaEventBatchDto.events", "Vec<MediaEventDto>");
    put("MediaEventBatchDto.next_sequence", "Option<u64>");
    put("MediaEventBatchDto.dropped_before_cursor", "bool");
    put("MediaEventDto.sequence", "u64");
    put("MediaEventDto.occurred_at_unix_ms", "u64");
    put("MediaEventDto.target", "MediaResourceRefDto");
    put("MediaEventDto.operation_id", "Option<MediaOperationIdDto>");
    put("MediaEventDto.kind", "MediaEventKindDto");
    put("MediaEventKindDto::StateChanged", "state_changed");
    put(
        "MediaEventKindDto::StateChanged.resource",
        "MediaResourceRefDto",
    );
    put(
        "MediaEventKindDto::StateChanged.state",
        "MediaResourceStateDto",
    );
    put("MediaEventKindDto::Frame", "frame");
    put("MediaEventKindDto::Frame.source", "MediaSourceRefDto");
    put("MediaEventKindDto::Frame.asset", "MediaAssetDto");
    put("MediaEventKindDto::Transcript", "transcript");
    put("MediaEventKindDto::Transcript.text", "String");
    put("MediaEventKindDto::Transcript.language", "Option<String>");
    put("MediaEventKindDto::Transcript.confidence", "Option<f32>");
    put("MediaEventKindDto::Error", "error");
    put("MediaEventKindDto::Error.kind", "MediaErrorKindDto");
    put("MediaEventKindDto::Error.message", "String");
    put("MediaContentDto.media_ref", "String");
    put("MediaContentDto.metadata", "MediaMetadataDto");
    put("MediaContentDto.bytes", "Vec<u8>");
    put("MediaErrorDto.kind", "MediaErrorKindDto");
    put("MediaErrorDto.message", "String");
    for (variant, tag) in [
        ("PermissionDenied", "permission_denied"),
        ("Busy", "busy"),
        ("Cancelled", "cancelled"),
        ("Timeout", "timeout"),
        ("NoData", "no_data"),
        ("NotFound", "not_found"),
        ("Unavailable", "unavailable"),
        ("Unsupported", "unsupported"),
        ("ModelMissing", "model_missing"),
        ("InvalidRequest", "invalid_request"),
        ("ProcessingFailed", "processing_failed"),
        ("NativeFailure", "native_failure"),
        ("StoreFailure", "store_failure"),
        ("MediaTooLarge", "media_too_large"),
    ] {
        put(&format!("MediaErrorKindDto::{variant}"), tag);
    }
    put("MediaDeviceOpenRequestDto.identity", "MediaOperationIdDto");
    put("MediaDeviceOpenRequestDto.owner", "MediaOwnerDto");
    put(
        "MediaDeviceOpenRequestDto.initiator",
        "Option<MediaInitiatorDto>",
    );
    put("MediaDeviceOpenRequestDto.timeout_budget_ms", "Option<u64>");
    put("MediaDeviceOpenRequestDto.max_payload_bytes", "u64");
    put(
        "MediaDeviceOpenRequestDto.request",
        "MediaSessionRequestDto",
    );
    put("MediaDeviceLeaseDto.lease_id", "String");
    put("MediaPcmFrameDto.sample_rate_hz", "u32");
    put("MediaPcmFrameDto.channels", "u16");
    put("MediaPcmFrameDto.pcm16le", "Vec<u8>");
    put("MediaVideoFrameDto::Rgba8", "rgba8");
    put("MediaVideoFrameDto::Rgba8.width_px", "u32");
    put("MediaVideoFrameDto::Rgba8.height_px", "u32");
    put("MediaVideoFrameDto::Rgba8.stride_bytes", "u32");
    put("MediaVideoFrameDto::Rgba8.bytes", "Vec<u8>");
    put("MediaVideoFrameDto::I420", "i420");
    put("MediaVideoFrameDto::I420.width_px", "u32");
    put("MediaVideoFrameDto::I420.height_px", "u32");
    put("MediaVideoFrameDto::I420.y_stride_bytes", "u32");
    put("MediaVideoFrameDto::I420.u_stride_bytes", "u32");
    put("MediaVideoFrameDto::I420.v_stride_bytes", "u32");
    put("MediaVideoFrameDto::I420.y", "Vec<u8>");
    put("MediaVideoFrameDto::I420.u", "Vec<u8>");
    put("MediaVideoFrameDto::I420.v", "Vec<u8>");
    put("MediaFramePayloadDto::Audio", "audio");
    put("MediaFramePayloadDto::Audio.frame", "MediaPcmFrameDto");
    put("MediaFramePayloadDto::Video", "video");
    put("MediaFramePayloadDto::Video.frame", "MediaVideoFrameDto");
    put("MediaFrameDto.source", "MediaSourceRefDto");
    put("MediaFrameDto.sequence", "u64");
    put("MediaFrameDto.captured_at_unix_ms", "u64");
    put("MediaFrameDto.payload", "MediaFramePayloadDto");
    put("MediaRenderReceiptDto.accepted_sample_frames", "u32");
    put("MediaRenderReceiptDto.played_sample_frames", "u32");
    put("MediaRenderReceiptDto.rendered_at_unix_ms", "Option<u64>");
    put("MediaDeviceUpdateRequestDto.lease_id", "String");
    put(
        "MediaDeviceUpdateRequestDto.update",
        "MediaSessionUpdateDto",
    );
    put("MediaDeviceAudioWriteRequestDto.lease_id", "String");
    put("MediaDeviceAudioWriteRequestDto.frame", "MediaPcmFrameDto");
    put("MediaDeviceVideoWriteRequestDto.lease_id", "String");
    put(
        "MediaDeviceVideoWriteRequestDto.frame",
        "MediaVideoFrameDto",
    );
    put("MediaDeviceAudioControlRequestDto.lease_id", "String");
    for (variant, tag) in [
        ("Open", "open"),
        ("NextInput", "next_input"),
        ("WriteAudio", "write_audio"),
        ("WriteVideo", "write_video"),
        ("FlushAudio", "flush_audio"),
        ("InterruptAudio", "interrupt_audio"),
        ("Update", "update"),
        ("Stop", "stop"),
    ] {
        put(&format!("MediaDeviceRequestDto::{variant}"), tag);
    }
    put(
        "MediaDeviceRequestDto::Open.request",
        "MediaDeviceOpenRequestDto",
    );
    put("MediaDeviceRequestDto::NextInput.lease_id", "String");
    put(
        "MediaDeviceRequestDto::WriteAudio.request",
        "MediaDeviceAudioWriteRequestDto",
    );
    put(
        "MediaDeviceRequestDto::WriteVideo.request",
        "MediaDeviceVideoWriteRequestDto",
    );
    put(
        "MediaDeviceRequestDto::FlushAudio.request",
        "MediaDeviceAudioControlRequestDto",
    );
    put(
        "MediaDeviceRequestDto::InterruptAudio.request",
        "MediaDeviceAudioControlRequestDto",
    );
    put(
        "MediaDeviceRequestDto::Update.request",
        "MediaDeviceUpdateRequestDto",
    );
    put("MediaDeviceRequestDto::Stop.lease_id", "String");
    put("MediaDeviceRequestEnvelopeDto.request_id", "String");
    put(
        "MediaDeviceRequestEnvelopeDto.request",
        "MediaDeviceRequestDto",
    );
    put("MediaDeviceResponseDto.request_id", "String");
    put("MediaDeviceResponseDto.result", "MediaDeviceResultDto");
    put("MediaDeviceResultDto::Opened", "opened");
    put("MediaDeviceResultDto::Opened.lease", "MediaDeviceLeaseDto");
    put("MediaDeviceResultDto::Input", "input");
    put("MediaDeviceResultDto::Input.frame", "Option<MediaFrameDto>");
    put("MediaDeviceResultDto::AudioWritten", "audio_written");
    put(
        "MediaDeviceResultDto::AudioWritten.receipt",
        "MediaRenderReceiptDto",
    );
    put("MediaDeviceResultDto::AudioFlushed", "audio_flushed");
    put(
        "MediaDeviceResultDto::AudioFlushed.receipt",
        "MediaRenderReceiptDto",
    );
    put(
        "MediaDeviceResultDto::AudioInterrupted",
        "audio_interrupted",
    );
    put(
        "MediaDeviceResultDto::AudioInterrupted.receipt",
        "MediaRenderReceiptDto",
    );
    put("MediaDeviceResultDto::VideoWritten", "video_written");
    put("MediaDeviceResultDto::Updated", "updated");
    put(
        "MediaDeviceResultDto::Updated.status",
        "MediaSessionStatusDto",
    );
    put("MediaDeviceResultDto::Stopped", "stopped");
    put("MediaDeviceResultDto::Error", "error");
    put("MediaDeviceResultDto::Error.error", "MediaErrorDto");
    put("MediaSessionDescriptionTypeDto::Offer", "offer");
    put("MediaSessionDescriptionTypeDto::Answer", "answer");
    put(
        "MediaSessionDescriptionDto.kind",
        "MediaSessionDescriptionTypeDto",
    );
    put("MediaSessionDescriptionDto.sdp", "String");
    put("MediaConnectionRequestDto.request_id", "String");
    put("MediaConnectionRequestDto.connection_id", "String");
    put("MediaConnectionRequestDto.session_id", "String");
    put(
        "MediaConnectionRequestDto.media_session_ref",
        "MediaSessionRefDto",
    );
    put(
        "MediaConnectionRequestDto.operation_id",
        "MediaOperationIdDto",
    );
    put("MediaConnectionRequestDto.owner", "MediaOwnerDto");
    put("MediaConnectionRequestDto.mode", "MediaSessionModeDto");
    put("MediaConnectionRequestDto.intent", "MediaSessionIntentDto");
    put(
        "MediaConnectionRequestDto.audio_source",
        "Option<MediaSourceRefDto>",
    );
    put(
        "MediaConnectionRequestDto.video_source",
        "Option<MediaSourceRefDto>",
    );
    put(
        "MediaConnectionRequestDto.description",
        "MediaSessionDescriptionDto",
    );
    put("MediaConnectionResponseDto.request_id", "String");
    put("MediaConnectionResponseDto.connection_id", "String");
    put(
        "MediaConnectionResponseDto.media_session_ref",
        "MediaSessionRefDto",
    );
    put(
        "MediaConnectionResponseDto.description",
        "Option<MediaSessionDescriptionDto>",
    );
    put("MediaConnectionResponseDto.error", "Option<MediaErrorDto>");
    put("MediaControlActionDto::FlushAudio", "flush_audio");
    put("MediaControlActionDto::FlushAudio.target_sequence", "u64");
    put(
        "MediaControlActionDto::FlushAudio.target_sample_frames",
        "u64",
    );
    put("MediaControlActionDto::InterruptAudio", "interrupt_audio");
    put(
        "MediaControlActionDto::InterruptAudio.target_sequence",
        "u64",
    );
    put(
        "MediaControlActionDto::InterruptAudio.target_sample_frames",
        "u64",
    );
    put("MediaControlActionDto::Close", "close");
    put("MediaControlRequestDto.request_id", "String");
    put("MediaControlRequestDto.connection_id", "String");
    put(
        "MediaControlRequestDto.media_session_ref",
        "MediaSessionRefDto",
    );
    put("MediaControlRequestDto.generation", "u64");
    put("MediaControlRequestDto.action", "MediaControlActionDto");
    put("MediaControlResponseDto.request_id", "String");
    put("MediaControlResponseDto.connection_id", "String");
    put(
        "MediaControlResponseDto.media_session_ref",
        "MediaSessionRefDto",
    );
    put("MediaControlResponseDto.generation", "u64");
    put(
        "MediaControlResponseDto.receipt",
        "Option<MediaRenderReceiptDto>",
    );
    put("MediaControlResponseDto.error", "Option<MediaErrorDto>");

    ix
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn workspace_event_retains_the_complete_directory_and_session_generation() {
    use client::protocol::{events::ClientEvent, listings::WorkspaceStateDto};
    let event = ClientEvent::WorkspaceChanged {
        state: WorkspaceStateDto {
            session_id: "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa".into(),
            cwd: "/home/lingxi/lingxi/projects/项目 B".into(),
            project_root: "/home/lingxi/lingxi/projects/项目 A".into(),
            additional_directories: vec!["/home/lingxi/linked".into()],
            transcript_path: "/home/lingxi/.lingxi/projects/b/session.jsonl".into(),
        },
    };
    let json = serde_json::to_string(&event).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).unwrap()["type"],
        "workspace_changed"
    );
    assert_eq!(serde_json::from_str::<ClientEvent>(&json).unwrap(), event);
}

#[test]
fn exact_client_text_keeps_the_protocol_json_string_shape() {
    let text = client::protocol::text::ClientTextDto::from_code_units(vec![b't' as u16, 0xd800]);
    let encoded = serde_json::to_string(&text).unwrap();
    assert_eq!(encoded, r#""t\ud800""#);
    let decoded: client::protocol::text::ClientTextDto = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.code_units, [b't' as u16, 0xd800]);
}

/// Removing a field is a BREAKING change ⇒ the classifier returns `Breaking`,
/// which (per the guard) forces a MAJOR `CLIENT_PROTOCOL_VERSION` bump.
///
/// Simulated by doctoring the current index: drop one field key and assert the
/// classifier flags it.
#[test]
fn removing_a_field_requires_major_bump() {
    let base = current_contract_index();

    let mut doctored = base.clone();
    let removed = doctored
        .remove("ClientEvent::ToolUseStarted.input_json")
        .expect("the field exists in the current contract index");
    assert_eq!(removed, "String");

    // old = full contract, new = the doctored (field-removed) contract.
    assert_eq!(
        classify(&base, &doctored),
        Compatibility::Breaking,
        "removing a field must classify as Breaking (forces a major bump)"
    );
}

/// Renaming a variant tag is BREAKING (it surfaces as a removed key + a new key,
/// and the removed key dominates).
#[test]
fn renaming_a_variant_requires_major_bump() {
    let base = current_contract_index();

    let mut doctored = base.clone();
    // Rename `ClientCommand::SetModel` → a different key (simulating a rename).
    doctored
        .remove("ClientCommand::SetModel")
        .expect("variant tag key exists");
    doctored.insert(
        "ClientCommand::ChangeModel".to_string(),
        "change_model".to_string(),
    );

    assert_eq!(
        classify(&base, &doctored),
        Compatibility::Breaking,
        "renaming a variant must classify as Breaking (forces a major bump)"
    );
}

/// Retyping a field (e.g. `u32` → `String`) is BREAKING.
#[test]
fn retyping_a_field_requires_major_bump() {
    let base = current_contract_index();

    let mut doctored = base.clone();
    doctored.insert(
        "SessionRowDto.message_count".to_string(),
        "String".to_string(), // was u32
    );

    assert_eq!(
        classify(&base, &doctored),
        Compatibility::Breaking,
        "retyping a field must classify as Breaking (forces a major bump)"
    );
}

/// Regression for the post-Task-2 review finding: the breaking-change
/// threshold must travel with the version, not be a literal pin. A literal
/// `current_major > 1` is true forever once the major has EVER been bumped
/// past 1 — so a SECOND breaking change landing while `CLIENT_PROTOCOL_VERSION`
/// is already e.g. `2.0.0`, with no further bump, must still be REJECTED.
/// `major_was_bumped_past` compares against the major blessed alongside the
/// checked-in index instead, so it keeps gating every subsequent breaking
/// change, not just the first one.
#[test]
fn a_second_breaking_change_with_no_further_bump_is_still_rejected() {
    // The exact failure sequence the review described: a first breaking
    // change bumped 1 -> 2 (blessed_major becomes 2). A SECOND breaking
    // change lands with CLIENT_PROTOCOL_VERSION still at major 2 — a literal
    // `current_major > 1` would wrongly pass (`2 > 1`); the real check must
    // reject it (`2` does not exceed the blessed `2`).
    assert!(
        !major_was_bumped_past(2, 2),
        "a second breaking change must still be rejected when the major did not move again"
    );
    // The properly-bumped case (2 -> 3) must pass.
    assert!(
        major_was_bumped_past(3, 2),
        "a genuine further bump past the blessed major must be accepted"
    );
    // The original (first-ever) bump this guard was written for must still work.
    assert!(
        major_was_bumped_past(2, 1),
        "the original 1 -> 2 bump must still be accepted"
    );
    // A missing/no-op bump at the foundation pin must still be rejected.
    assert!(
        !major_was_bumped_past(1, 1),
        "no bump at all must be rejected"
    );
}

/// Adding a NEW optional field is a COMPATIBLE (additive) change ⇒ the
/// classifier returns `Compatible`, so NO major bump is required.
#[test]
fn adding_optional_field_is_compatible() {
    let base = current_contract_index();

    let mut doctored = base.clone();
    // Add a brand-new optional field to an existing variant.
    doctored.insert(
        "ClientEvent::TextDelta.lang".to_string(),
        "Option<String>".to_string(),
    );

    // old = full contract (without the new field), new = with the new field.
    assert_eq!(
        classify(&base, &doctored),
        Compatibility::Compatible,
        "adding an optional field must classify as Compatible (no bump required)"
    );
}

/// Adding a NEW variant is also COMPATIBLE (the enums are `#[non_exhaustive]`,
/// so a new variant is additive).
#[test]
fn adding_a_variant_is_compatible() {
    let base = current_contract_index();

    let mut doctored = base.clone();
    doctored.insert(
        "ClientEvent::PromptAccepted".to_string(),
        "prompt_accepted".to_string(),
    );
    doctored.insert(
        "ClientEvent::PromptAccepted.prompt_id".to_string(),
        "String".to_string(),
    );

    assert_eq!(
        classify(&base, &doctored),
        Compatibility::Compatible,
        "adding a variant must classify as Compatible (no bump required)"
    );
}

/// Adding a required field to an already-existing variant/record changes the
/// payload shape for existing clients and therefore requires a major bump.
#[test]
fn adding_required_field_to_existing_variant_requires_major_bump() {
    let base = current_contract_index();

    let mut doctored = base.clone();
    doctored.insert(
        "ClientEvent::TextDelta.lang".to_string(),
        "String".to_string(),
    );

    assert_eq!(
        classify(&base, &doctored),
        Compatibility::Breaking,
        "adding a required field to an existing variant must require a major bump"
    );
}

/// An identical index is trivially COMPATIBLE.
#[test]
fn identical_index_is_compatible() {
    let ix = current_contract_index();
    assert_eq!(classify(&ix, &ix), Compatibility::Compatible);
}

/// THE GUARD: the current contract index either matches the checked-in one, OR,
/// if it changed in a BREAKING way, the major component of
/// `CLIENT_PROTOCOL_VERSION` must have been bumped past the checked-in major.
///
/// - Index byte-identical ⇒ pass (no contract drift).
/// - Index changed but `Compatible` (purely additive) ⇒ pass, then remind the
///   author to re-bless so the additive entries are recorded.
/// - Index changed in a `Breaking` way ⇒ pass ONLY if the major was bumped past
///   the checked-in major; otherwise fail.
///
/// Under `BLESS=1`, (re)write the index from the current contract. A bless is
/// legitimate only after classifying the change and bumping the major when the
/// change is breaking.
#[test]
fn current_contract_matches_index_or_version_bumped() {
    let current = current_contract_index();
    let current_major = major_of(CLIENT_PROTOCOL_VERSION);

    if bless() {
        write_index(&current);
        // Always in lockstep with the index, not just on a breaking bless:
        // the sidecar's job is to answer "what major was checked in", which
        // must stay true after an additive-only re-bless too.
        write_blessed_major(current_major);
        return;
    }

    let Some(checked_in) = read_checked_in_index() else {
        panic!(
            "missing contract index `{}`; regenerate with \
             `BLESS=1 cargo test -p client --test version_guard_test`",
            index_path().display()
        );
    };

    // Byte-identical contract → nothing to check.
    if checked_in == current {
        return;
    }

    // The contract changed. Classify the change.
    let verdict = classify(&checked_in, &current);

    match verdict {
        Compatibility::Compatible => {
            // Additive-only: this is allowed WITHOUT a major bump, but the
            // checked-in index must be refreshed so the additive entries are
            // recorded for the NEXT diff.
            panic!(
                "the contract grew in an ADDITIVE way (new variant / optional field) but \
                 `snapshots/contract_index.json` is stale. No major bump is required; \
                 re-bless the index with \
                 `BLESS=1 cargo test -p client --test version_guard_test`."
            );
        }
        Compatibility::Breaking => {
            // We need a major bump. THE THRESHOLD TRAVELS WITH THE VERSION:
            // compare against the major that was blessed alongside the
            // CHECKED-IN index (`blessed_major_path()`), never a literal pin
            // — a literal (e.g. `> 1`) is correct only until the FIRST bump
            // ever lands, then stays true forever and stops gating anything
            // (finding: this guard shipped with exactly that bug once
            // `CLIENT_PROTOCOL_VERSION` first became `2.x.x`).
            let blessed_major = read_blessed_major().unwrap_or_else(|| {
                panic!(
                    "missing `{}` (the major recorded at the last bless); regenerate with \
                     `BLESS=1 cargo test -p client --test version_guard_test`",
                    blessed_major_path().display()
                )
            });
            // NAME the entries. A bare "something broke" leaves the author
            // unable to tell a guard that saw their deletion from a guard that
            // is blind to it and tripped on an unrelated key.
            let broken = breaking_entries(&checked_in, &current);
            assert!(
                major_was_bumped_past(current_major, blessed_major),
                "BREAKING contract change detected but `CLIENT_PROTOCOL_VERSION`'s major \
                 ({current_major}, from {CLIENT_PROTOCOL_VERSION:?}) does not exceed the \
                 major blessed alongside the checked-in contract index ({blessed_major}, \
                 from `{}`). Per decision §0.10 a breaking change REQUIRES a major bump \
                 PAST the last blessed one. Bump the major in \
                 `client/src/protocol/version.rs`, then re-bless with \
                 `BLESS=1 cargo test -p client --test version_guard_test`.\n\
                 The {} breaking entr{} (checked-in index -> current contract):\n  {}",
                blessed_major_path().display(),
                broken.len(),
                if broken.len() == 1 { "y" } else { "ies" },
                broken.join("\n  ")
            );
        }
    }
}

#[test]
fn skill_admin_command_and_source_target_are_indexed() {
    let index = current_contract_index();
    assert_eq!(
        index.get("ClientCommand::SkillAdmin").map(String::as_str),
        Some("skill_admin")
    );
    assert_eq!(
        index
            .get("ClientCommand::SkillAdmin.command")
            .map(String::as_str),
        Some("SkillAdminCommandDto")
    );
    assert_eq!(
        index.get("SkillAdminCommandDto.target").map(String::as_str),
        Some("Option<ClientTextDto>")
    );
}

/// Compile-time / structural anchor: constructing one value of EVERY contract
/// type proves the index author saw every type. If a new DTO is added without a
/// matching index line, this constructor still compiles but
/// `current_contract_matches_index_or_version_bumped` flags the new key on the
/// first run after `BLESS=1` — and conversely, deleting a DTO breaks THIS
/// function's compile, so the index cannot silently keep a stale key.
///
/// (This is a belt-and-braces exhaustiveness check; the real guarantee is the
/// guard test above.)
#[test]
// Coverage anchors: each `let _foo = Dto::Variant{..}` instantiates a DTO so a
// removed/renamed type breaks THIS compile. They are intentionally inert (no
// effect, never read) — that is the whole point — so both pedantic lints are
// allowed here rather than worked around.
#[allow(clippy::too_many_lines, clippy::no_effect_underscore_binding)]
fn contract_index_covers_every_dto() {
    use client::protocol::audio::{
        AudioCapabilitySnapshotDto, AudioErrorDto, AudioErrorKindDto, AudioInitiatorDto,
        AudioOperationDto, AudioOperationIdDto, AudioOperationKindDto, AudioOperationReadinessDto,
        AudioOperationRequestDto, AudioOperationResultDto, AudioOwnerDto, AudioReadinessStateDto,
        AudioStatusDto,
    };
    use client::protocol::commands::{
        ClientCommand, CommandResultDto, ImageRefDto, ListingKindDto, PermissionBehaviorDto,
        PromptModeDto, ProviderCredentialSecretDto, SkillAdminCommandDto, WritableScopeDto,
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
        ClientEvent, CostDto, ErrorKindDto, RefusalContinuationJoinDto,
        RefusalContinuationPhaseDto, ServerFallbackProviderMessageDto,
        ServerFallbackTombstoneMessageDto, TurnOutcomeDto, TurnRecoverySnapshotDto,
        TurnRecoveryStateDto,
    };
    use client::protocol::listings::{
        AgentDto, AuthStateDto, CheckStatusDto, CoordinatorWorkerDto, DoctorCheckDto,
        DoctorReportDto, DoctorSummaryDto, HookDto, McpServerDto, McpStatusDto, MemoryEntryDto,
        MemoryTierDto, ModelContextBudgetDto, SessionAgentMessageRowDto, SessionAgentSummaryDto,
        SessionRowDto, SkillDto, SlashCommandDto, StatusSnapshotDto, TaskRowDto, TaskStatusDto,
    };
    use client::protocol::media::*;
    use client::protocol::message::{MessageBlockDto, MessageDto};
    use client::protocol::permission::{
        PermissionKindDto, PermissionRequest, PermissionResolved, PermissionResponseDto,
        WorkerInfoDto,
    };
    use client::protocol::text::ClientTextDto;
    use client::protocol::tool_display::{
        CodeSegmentDto, DiffLineKindDto, DiffRowDto, HeadlineKindDto, PlanTaskDto,
        PlanTaskStateDto, StructuredDiffDto, SyntaxClassDto, ToolHeaderDto, ToolResultDisplayDto,
        ToolSubLineDto, ToolVerbDto,
    };

    // Touch every type so removing a type breaks this compile. The values are
    // intentionally minimal; this is a coverage anchor, not a round-trip test.
    let _text = ClientTextDto::default();
    let _context_budget = ModelContextBudgetDto {
        policy: String::new(),
        effective_input_budget: None,
        hard_input_limit: None,
        standard_price_input_tokens: None,
        recommendation_input_tokens: None,
        recommendation_source: None,
    };
    let _ev: Vec<ClientEvent> = vec![
        ClientEvent::WorkspaceChanged {
            state: client::protocol::listings::WorkspaceStateDto {
                session_id: String::new(),
                cwd: String::new(),
                project_root: String::new(),
                additional_directories: Vec::new(),
                transcript_path: String::new(),
            },
        },
        ClientEvent::Error {
            kind: ErrorKindDto::Internal,
            message: String::new().into(),
        },
        ClientEvent::TerminalApiError {
            api_error_status: Some(413),
            terminal_reason: "image_error".into(),
            stop_reason: "stop_sequence".into(),
        },
        ClientEvent::SystemNotice {
            message: String::new().into(),
            is_error: false,
        },
        ClientEvent::UiLog {
            plugin: String::new(),
            text: ClientTextDto::default(),
        },
        ClientEvent::UiToast {
            plugin: String::new(),
            text: ClientTextDto::default(),
            timeout_ms: 4000,
        },
        ClientEvent::UiStatus {
            plugin: String::new(),
            text: None,
        },
        ClientEvent::UiControlResult {
            request_id: String::new(),
            response_json: None,
            metadata_json: None,
            error: Some(String::new()),
        },
        ClientEvent::UiClientFrame {
            runtime_id: String::new(),
            frame_json: String::new(),
        },
        ClientEvent::PromptSuggestion {
            suggestion: String::new().into(),
            uuid: String::new(),
            session_id: String::new(),
        },
        ClientEvent::PromptSuggestionState {
            enabled: true,
            notice: None,
        },
        ClientEvent::UiInvalidate {
            instances_json: None,
            uuid: String::new(),
            session_id: String::new(),
        },
        ClientEvent::UiNotify {
            plugin: String::new(),
            text: ClientTextDto::default(),
        },
        ClientEvent::VisualizationBlock {
            status: client::protocol::message::VisualizationBlockStatusDto::Pending,
            reference: Some(client::protocol::message::VisualizationRefDto {
                id: String::new(),
                revision: 1,
            }),
        },
        ClientEvent::ProviderCredentialStatus {
            operation_id: 0,
            configured_provider_ids: Vec::new(),
            unavailable_provider_ids: Vec::new(),
            storage_encrypted: false,
            credential_previews: std::collections::HashMap::new(),
            error: None,
        },
        ClientEvent::ConversationControlsChanged {
            controls: ConversationControlsDto {
                qualified_model: String::new(),
                permission: PermissionControlStateDto {
                    requested: String::new(),
                    effective: String::new(),
                    options: vec![PermissionModeOptionDto {
                        mode: String::new(),
                        available: false,
                        disabled_reason: Some(ControlDisabledReasonDto {
                            code: String::new(),
                            message: None,
                        }),
                    }],
                },
                reasoning: ReasoningControlStateDto {
                    requested: ReasoningSelectionDto::Automatic,
                    effective: ReasoningSelectionDto::Level { id: String::new() },
                    spec: ReasoningControlSpecDto {
                        options: vec![ReasoningOptionDto {
                            selection: ReasoningSelectionDto::TokenBudget { tokens: 0 },
                            persistable: false,
                        }],
                        budget_range: Some(ReasoningBudgetRangeDto {
                            min_tokens: 0,
                            max_tokens: 0,
                        }),
                        provider_default: ReasoningSelectionDto::Enabled,
                        forced_reasoning: false,
                        editable: true,
                        disabled_reason: None,
                    },
                },
                proactivity: None,
            },
        },
        ClientEvent::SessionEnded,
        ClientEvent::TurnRecoveryState {
            snapshot: TurnRecoverySnapshotDto {
                session_id: String::new(),
                turn_id: 0,
                revision: 1,
                state: TurnRecoveryStateDto::Running,
                first_sequence: 0,
                last_sequence: 0,
                safe_to_resume: true,
                projection_uncertain: false,
                reason: None,
            },
        },
        ClientEvent::TurnEventReplay {
            session_id: String::new(),
            turn_id: 0,
            sequence: 0,
            event_json: String::new(),
        },
        ClientEvent::TurnEventLiveUnretained {
            session_id: String::new(),
            turn_id: 0,
            event_json: String::new(),
        },
        ClientEvent::TurnAttachStarted {
            request_id: String::new(),
            snapshot: TurnRecoverySnapshotDto {
                session_id: String::new(),
                turn_id: 0,
                revision: 0,
                state: TurnRecoveryStateDto::Running,
                first_sequence: 0,
                last_sequence: 0,
                safe_to_resume: false,
                projection_uncertain: false,
                reason: None,
            },
        },
        ClientEvent::TurnAttachFinished {
            request_id: String::new(),
            session_id: String::new(),
            turn_id: 0,
        },
        ClientEvent::QueuedTurnStatus {
            session_id: String::new(),
            turn_id: 0,
            request_id: String::new(),
            status: client::protocol::events::QueuedTurnStatusDto::Delivered {
                batch_owner_turn_id: 0,
                member_turn_ids: Vec::new(),
                transcript_uuid: String::new(),
            },
        },
        ClientEvent::ModSessionEvent {
            session_id: String::new(),
            mod_turn_id: None,
            payload: client::protocol::mod_session_event::ModSessionEventPayload::Status {
                plugin: String::new(),
                text: None,
            },
        },
        ClientEvent::AudioSessionContext {
            session_id: String::new(),
            profile_id: String::new(),
            account_scope: String::new(),
        },
        ClientEvent::RealtimeAudioEvent {
            session_id: String::new(),
            event_json: String::new(),
        },
        ClientEvent::AudioRequest {
            request: AudioOperationRequestDto {
                identity: AudioOperationIdDto {
                    id: String::new(),
                    generation: 0,
                    service_epoch: 0,
                },
                owner: AudioOwnerDto::Session {
                    session_id: String::new(),
                },
                initiator: None,
                timeout_budget_ms: None,
                route_policy: None,
                max_payload_bytes: 0,
                operation: AudioOperationDto::Status { handle: None },
            },
        },
        ClientEvent::AudioCancel {
            identity: AudioOperationIdDto {
                id: String::new(),
                generation: 0,
                service_epoch: 0,
            },
        },
        ClientEvent::AudioCapabilitiesChanged {
            capabilities: AudioCapabilitySnapshotDto {
                service_epoch: 0,
                support_revision: 0,
                supported_operations: Vec::new(),
                readiness: Vec::new(),
                max_payload_bytes: 0,
            },
        },
        ClientEvent::MediaConnectionRequest {
            request: MediaConnectionRequestDto {
                request_id: String::new(),
                connection_id: String::new(),
                session_id: String::new(),
                media_session_ref: MediaSessionRefDto { id: String::new() },
                operation_id: MediaOperationIdDto {
                    id: String::new(),
                    generation: 0,
                    service_epoch: 0,
                },
                owner: MediaOwnerDto::Session {
                    session_id: String::new(),
                },
                mode: MediaSessionModeDto::Audio,
                intent: MediaSessionIntentDto::Observation,
                audio_source: None,
                video_source: None,
                description: MediaSessionDescriptionDto {
                    kind: MediaSessionDescriptionTypeDto::Offer,
                    sdp: String::new(),
                },
            },
        },
        ClientEvent::MediaDeviceRequest {
            request: MediaDeviceRequestEnvelopeDto {
                request_id: String::new(),
                request: MediaDeviceRequestDto::Open {
                    request: MediaDeviceOpenRequestDto {
                        identity: MediaOperationIdDto {
                            id: String::new(),
                            generation: 0,
                            service_epoch: 0,
                        },
                        owner: MediaOwnerDto::Session {
                            session_id: String::new(),
                        },
                        initiator: None,
                        timeout_budget_ms: None,
                        max_payload_bytes: 0,
                        request: MediaSessionRequestDto {
                            mode: MediaSessionModeDto::Audio,
                            intent: MediaSessionIntentDto::Observation,
                            audio_source: None,
                            video_source: None,
                        },
                    },
                },
            },
        },
        ClientEvent::MediaServiceResponse {
            response: MediaServiceResponseDto {
                request_id: String::new(),
                result: None,
                error: Some(MediaErrorDto {
                    kind: MediaErrorKindDto::Unsupported,
                    message: String::new(),
                }),
            },
        },
        ClientEvent::MediaControlRequest {
            request: MediaControlRequestDto {
                request_id: String::new(),
                connection_id: String::new(),
                media_session_ref: MediaSessionRefDto { id: String::new() },
                generation: 0,
                action: MediaControlActionDto::FlushAudio {
                    target_sequence: 0,
                    target_sample_frames: 0,
                },
            },
        },
        ClientEvent::UserTranscriptRowIdentity {
            row_token: String::new(),
            uuid: String::new(),
        },
        ClientEvent::AssistantTranscriptRowUuids {
            message_id: String::new(),
            uuids: Vec::new(),
        },
        ClientEvent::QueryModelChange {
            to_model: String::new(),
        },
        ClientEvent::AssistantBlockStart { block_key: 0 },
        ClientEvent::AssistantBlockIdentity {
            block_key: 0,
            message_uuid: String::new(),
        },
        ClientEvent::Tombstone {
            message: ServerFallbackTombstoneMessageDto {
                uuid: String::new(),
                message_type: String::new(),
                timestamp: String::new(),
                request_id: None,
                request_ref_json: None,
                message: ServerFallbackProviderMessageDto {
                    id: None,
                    model: None,
                    stop_reason: None,
                    stop_details_json: None,
                    usage_json: None,
                    content_json: "[]".into(),
                },
                is_api_error_message: None,
                supersedes_uuids: None,
            },
            display_only: true,
        },
        ClientEvent::RefusalContinuation {
            phase: RefusalContinuationPhaseDto::Begin,
            salvage_text: String::new(),
            join: RefusalContinuationJoinDto::Exact,
            replaces_uuids: Vec::new(),
            display_salvage_text: true,
        },
    ];
    let _outcome = TurnOutcomeDto::EndTurn;
    // Every AudioOperationDto variant, so a removed operation fails THIS compile.
    let _audio_ops: Vec<AudioOperationDto> = vec![
        AudioOperationDto::Capture {
            sample_rate_hz: 24_000,
            format: "m4a".into(),
        },
        AudioOperationDto::Play {
            pcm_base64: String::new(),
            sample_rate_hz: 24_000,
        },
        AudioOperationDto::Transcribe {
            audio_base64: String::new(),
            mime_type: String::new(),
            language: None,
        },
        AudioOperationDto::StartRecording {
            sample_rate_hz: 16_000,
            format: String::new(),
        },
        AudioOperationDto::StopRecording {
            handle: String::new(),
        },
        AudioOperationDto::Listen { language: None },
        AudioOperationDto::Synthesize {
            text: String::new(),
            language: None,
            rate: None,
            voice: None,
        },
        AudioOperationDto::Speak {
            text: String::new(),
            language: None,
            rate: None,
            voice: None,
        },
        AudioOperationDto::Status { handle: None },
        AudioOperationDto::EndOwner,
    ];

    // tool_display.rs — the pre-derived render model.
    let _verb = ToolVerbDto::Update;
    let _sub = ToolSubLineDto {
        prefix: String::new(),
        text: String::new(),
    };
    let _header = ToolHeaderDto {
        verb: ToolVerbDto::Generic,
        icon: None,
        label: String::new(),
        primary: None,
        qualifier: None,
        count: None,
        sub_line: None,
        title: String::new(),
    };
    let _class = SyntaxClassDto::Plain;
    let _kind = DiffLineKindDto::Add;
    let _segment = CodeSegmentDto {
        text: String::new(),
        class: SyntaxClassDto::Plain,
        rgb: None,
        bold: false,
        italic: false,
        underline: false,
        emph: false,
    };
    let _row = DiffRowDto {
        kind: DiffLineKindDto::Context,
        line_no: 0,
        hunk: 0,
        word_diffed: false,
        segments: vec![],
    };
    let _diff = StructuredDiffDto {
        file_path: None,
        language: None,
        gutter_width: 0,
        additions: 0,
        removals: 0,
        truncated_rows: 0,
        rows: vec![],
    };
    let _headline_kind = HeadlineKindDto::Added;
    let _display = ToolResultDisplayDto {
        headline: None,
        headline_kind: None,
        headline_args: vec![],
        diff: None,
        body: None,
        body_lines: 0,
        body_truncated: false,
        collapsed: false,
    };
    let _plan_state = PlanTaskStateDto::Pending;
    let _plan_task = PlanTaskDto {
        id: None,
        subject: String::new(),
        active_form: None,
        state: PlanTaskStateDto::Pending,
    };
    let _cost = CostDto {
        total_usd: 0.0,
        input_tokens: 0,
        output_tokens: 0,
        api_calls: 0,
        session_duration_secs: 0,
        formatted: String::new(),
    };
    let skill_admin_command = SkillAdminCommandDto {
        action: "get_catalog".into(),
        operation_id: None,
        target: Some(ClientTextDto::default()),
        scope: None,
        revision: None,
        payload_json: None,
    };
    let _skill_admin = skill_admin_command.clone();
    let _cmd: Vec<ClientCommand> = vec![
        ClientCommand::SendPrompt {
            session_id: Some(String::new()),
            text: ClientTextDto::default(),
            prompt_mode: None,
            images: Vec::new(),
            turn_id: None,
            visualization_context: None,
        },
        ClientCommand::RunSlashCommand {
            session_id: Some(String::new()),
            raw: ClientTextDto::default(),
            images: Vec::new(),
            turn_id: None,
        },
        ClientCommand::ListProviderCredentials {
            operation_id: 0,
            provider_ids: Vec::new(),
            preview_provider_ids: Vec::new(),
        },
        ClientCommand::SetProviderCredential {
            operation_id: 0,
            provider_id: String::new(),
            credential: ProviderCredentialSecretDto::new(String::new()),
        },
        ClientCommand::DeleteProviderCredential {
            operation_id: 0,
            provider_id: String::new(),
        },
        ClientCommand::ListModels,
        ClientCommand::RequestExit,
        ClientCommand::SkillAdmin {
            command: skill_admin_command,
        },
        ClientCommand::GetConversationControls,
        ClientCommand::SetReasoningSelection {
            selection: ReasoningSelectionDto::Automatic,
        },
        ClientCommand::SetFastMode { enabled: false },
        ClientCommand::ApproveComputerAccess {
            request_id: 0,
            response: ComputerAccessResponseDto::default(),
        },
        ClientCommand::DenyComputerAccess { request_id: 0 },
        ClientCommand::AttachTurn {
            session_id: String::new(),
            turn_id: 0,
            after_sequence: None,
            request_id: String::new(),
        },
        ClientCommand::QueryQueuedTurn {
            session_id: String::new(),
            turn_id: 0,
            request_id: String::new(),
        },
        ClientCommand::Cancel {
            session_id: None,
            turn_id: None,
        },
        ClientCommand::ResumeTurn {
            session_id: String::new(),
            turn_id: 0,
        },
        ClientCommand::PauseTurn {
            session_id: String::new(),
            turn_id: 0,
            reason: String::new(),
        },
        ClientCommand::UpdateSettings {
            destination: WritableScopeDto::User,
            patch_json: String::new(),
        },
        ClientCommand::UpdatePermissionRules {
            destination: WritableScopeDto::User,
            behavior: PermissionBehaviorDto::Allow,
            add: Vec::new(),
            remove: Vec::new(),
        },
        ClientCommand::SetDefaultPermissionMode {
            destination: WritableScopeDto::User,
            mode: String::new(),
        },
        ClientCommand::UpdateWorkspaceDirectories {
            destination: WritableScopeDto::User,
            add: Vec::new(),
            remove: Vec::new(),
        },
        ClientCommand::UpsertMcpServer {
            scope: WritableScopeDto::Project,
            name: String::new(),
            config_json: String::new(),
        },
        ClientCommand::RemoveMcpServer {
            scope: WritableScopeDto::Project,
            name: String::new(),
        },
        ClientCommand::GetAudioSessionContext,
        ClientCommand::StartRealtimeAudio {
            request_json: String::new(),
        },
        ClientCommand::RealtimeAudioInput {
            input_json: String::new(),
        },
        ClientCommand::StopRealtimeAudio,
        ClientCommand::AudioResponse {
            identity: AudioOperationIdDto {
                id: String::new(),
                generation: 0,
                service_epoch: 0,
            },
            result: AudioOperationResultDto::OwnerEnded,
        },
        ClientCommand::UpdateAudioCapabilities {
            capabilities: AudioCapabilitySnapshotDto {
                service_epoch: 0,
                support_revision: 0,
                supported_operations: Vec::new(),
                readiness: Vec::new(),
                max_payload_bytes: 0,
            },
        },
        ClientCommand::MediaConnectionResponse {
            response: MediaConnectionResponseDto {
                request_id: String::new(),
                connection_id: String::new(),
                media_session_ref: MediaSessionRefDto { id: String::new() },
                description: None,
                error: Some(MediaErrorDto {
                    kind: MediaErrorKindDto::Unavailable,
                    message: String::new(),
                }),
            },
        },
        ClientCommand::MediaDeviceResponse {
            response: MediaDeviceResponseDto {
                request_id: String::new(),
                result: MediaDeviceResultDto::Stopped,
            },
        },
        ClientCommand::MediaServiceRequest {
            request: MediaServiceRequestDto {
                request_id: String::new(),
                action: MediaServiceActionDto::ReadMedia {
                    media_ref: String::new(),
                },
            },
        },
        ClientCommand::MediaControlResponse {
            response: MediaControlResponseDto {
                request_id: String::new(),
                connection_id: String::new(),
                media_session_ref: MediaSessionRefDto { id: String::new() },
                generation: 1,
                receipt: None,
                error: Some(MediaErrorDto {
                    kind: MediaErrorKindDto::Unavailable,
                    message: String::new(),
                }),
            },
        },
        ClientCommand::UiRender {
            request_id: String::new(),
            request_json: String::new(),
        },
        ClientCommand::UiClientModule {
            request_id: String::new(),
            plugin: String::new(),
        },
        ClientCommand::UiMessage {
            request_id: String::new(),
            request_json: String::new(),
        },
        ClientCommand::UiClientFault {
            request_id: String::new(),
            request_json: String::new(),
        },
        ClientCommand::UiClientPress {
            request_id: String::new(),
            request_json: String::new(),
        },
        ClientCommand::UiPress {
            request_id: String::new(),
            request_json: String::new(),
        },
        ClientCommand::UiInput {
            request_id: String::new(),
            request_json: String::new(),
        },
        ClientCommand::UiSelect {
            request_id: String::new(),
            request_json: String::new(),
        },
        ClientCommand::UpdatePromptSuggestionContext {
            session_id: String::new(),
            focus: "focused".into(),
            input_empty: true,
            pending_permission: false,
            elicitation_active: false,
        },
        ClientCommand::RecordPromptSuggestionSubmission {
            text: String::new(),
        },
        ClientCommand::PromptSuggestionFeedback {
            session_id: String::new(),
            uuid: String::new(),
            action: "shown".into(),
        },
        ClientCommand::UiClientOperation {
            request_id: String::new(),
            operation_json: String::new(),
        },
        ClientCommand::UpdateMediaSessionModes { modes: Vec::new() },
    ];
    // Every operation result and error variant must remain constructible.
    let _audio_results: Vec<AudioOperationResultDto> = vec![
        AudioOperationResultDto::RecordingStarted {
            handle: String::new(),
        },
        AudioOperationResultDto::Recording {
            audio_base64: String::new(),
            mime_type: String::new(),
        },
        AudioOperationResultDto::Transcript {
            text: String::new(),
            language: None,
            confidence: None,
        },
        AudioOperationResultDto::Synthesized {
            pcm_base64: String::new(),
            sample_rate_hz: 0,
        },
        AudioOperationResultDto::PlaybackCompleted { duration_ms: 0 },
        AudioOperationResultDto::Status {
            status: AudioStatusDto {
                recording: false,
                playing: false,
            },
        },
        AudioOperationResultDto::OwnerEnded,
        AudioOperationResultDto::Failed {
            error: AudioErrorDto {
                kind: AudioErrorKindDto::NativeFailure,
                message: String::new(),
            },
        },
    ];
    // Every AudioErrorKindDto variant, so a removed class fails this compile.
    let _audio_error_kinds: Vec<AudioErrorKindDto> = vec![
        AudioErrorKindDto::PermissionDenied,
        AudioErrorKindDto::Busy,
        AudioErrorKindDto::Cancelled,
        AudioErrorKindDto::Timeout,
        AudioErrorKindDto::NoSpeech,
        AudioErrorKindDto::NotRecording,
        AudioErrorKindDto::Unavailable,
        AudioErrorKindDto::Unsupported,
        AudioErrorKindDto::ModelMissing,
        AudioErrorKindDto::VoiceMissing,
        AudioErrorKindDto::InvalidRequest,
        AudioErrorKindDto::SynthesisFailed,
        AudioErrorKindDto::NativeFailure,
        AudioErrorKindDto::MediaTooLarge,
    ];
    let _audio_owners = [
        AudioOwnerDto::Session {
            session_id: String::new(),
        },
        AudioOwnerDto::Ui {
            instance_id: String::new(),
        },
        AudioOwnerDto::System {
            instance_id: String::new(),
        },
    ];
    let _audio_initiator = AudioInitiatorDto {
        agent_id: None,
        tool_use_id: None,
        request_id: None,
    };
    let _audio_kinds = [
        AudioOperationKindDto::Capture,
        AudioOperationKindDto::Play,
        AudioOperationKindDto::Transcribe,
        AudioOperationKindDto::Record,
        AudioOperationKindDto::Listen,
        AudioOperationKindDto::Synthesize,
        AudioOperationKindDto::Speak,
    ];
    let _audio_readiness = [
        AudioReadinessStateDto::Ready,
        AudioReadinessStateDto::NeedsPermission,
        AudioReadinessStateDto::Busy,
        AudioReadinessStateDto::MissingModel,
        AudioReadinessStateDto::Unavailable,
    ];
    let _audio_readiness_entry = AudioOperationReadinessDto {
        operation: AudioOperationKindDto::Listen,
        state: AudioReadinessStateDto::Busy,
    };
    let media_operation_id = MediaOperationIdDto {
        id: String::new(),
        generation: 1,
        service_epoch: 1,
    };
    let media_source_ref = MediaSourceRefDto {
        id: "source".into(),
    };
    let media_session_ref = MediaSessionRefDto {
        id: "session".into(),
    };
    let media_owner = MediaOwnerDto::Session {
        session_id: "session".into(),
    };
    let media_metadata = MediaMetadataDto {
        kind: MediaKindDto::Audio,
        mime_type: "audio/wav".into(),
        byte_len: 0,
        captured_at_unix_ms: None,
        duration_ms: None,
        width_px: None,
        height_px: None,
        sample_rate_hz: Some(24_000),
        channels: Some(1),
    };
    let media_asset = MediaAssetDto {
        media_ref: "asset".into(),
        metadata: media_metadata.clone(),
    };
    let media_target = MediaResourceRefDto::Session {
        session_id: "session".into(),
    };
    let media_session_status = MediaSessionStatusDto {
        state: MediaSessionStateDto::Ready,
        audio_enabled: true,
        video_enabled: false,
    };
    let media_session_request = MediaSessionRequestDto {
        mode: MediaSessionModeDto::Audio,
        intent: MediaSessionIntentDto::Conversation,
        audio_source: Some(media_source_ref.clone()),
        video_source: None,
    };
    let media_event = MediaEventDto {
        sequence: 1,
        occurred_at_unix_ms: 1,
        target: media_target.clone(),
        operation_id: Some(media_operation_id.clone()),
        kind: MediaEventKindDto::StateChanged {
            resource: media_target.clone(),
            state: MediaResourceStateDto::Active,
        },
    };
    let media_event_batch = MediaEventBatchDto {
        target: media_target.clone(),
        events: vec![media_event.clone()],
        next_sequence: Some(1),
        dropped_before_cursor: false,
    };
    let media_error = MediaErrorDto {
        kind: MediaErrorKindDto::Unavailable,
        message: String::new(),
    };
    let _media_capabilities = MediaCapabilitiesDto {
        service_epoch: 1,
        support_revision: 1,
        supported_operations: vec![MediaOperationKindDto::Sources],
        supported_media_kinds: vec![MediaKindDto::Audio],
        readiness: vec![MediaOperationReadinessDto {
            operation: MediaOperationKindDto::Sources,
            state: MediaReadinessStateDto::Ready,
        }],
        max_payload_bytes: 1024,
    };
    let _media_sources = [
        MediaSourceKindDto::Microphone,
        MediaSourceKindDto::Camera,
        MediaSourceKindDto::Screen,
        MediaSourceKindDto::Speaker,
        MediaSourceKindDto::Library,
        MediaSourceKindDto::Remote,
    ];
    let _media_usages = [
        MediaSourceUsageDto::Capture,
        MediaSourceUsageDto::Record,
        MediaSourceUsageDto::Transcribe,
        MediaSourceUsageDto::Play,
        MediaSourceUsageDto::Speak,
        MediaSourceUsageDto::Session,
        MediaSourceUsageDto::ReadImage,
    ];
    let _media_operation_kinds = [
        MediaOperationKindDto::Sources,
        MediaOperationKindDto::Status,
        MediaOperationKindDto::Capture,
        MediaOperationKindDto::Record,
        MediaOperationKindDto::Transcribe,
        MediaOperationKindDto::Synthesize,
        MediaOperationKindDto::Play,
        MediaOperationKindDto::Speak,
        MediaOperationKindDto::SessionStart,
        MediaOperationKindDto::SessionUpdate,
        MediaOperationKindDto::Observe,
        MediaOperationKindDto::Wait,
        MediaOperationKindDto::Stop,
    ];
    let _media_readiness_states = [
        MediaReadinessStateDto::Ready,
        MediaReadinessStateDto::NeedsPermission,
        MediaReadinessStateDto::Busy,
        MediaReadinessStateDto::MissingModel,
        MediaReadinessStateDto::Unavailable,
    ];
    let _media_source = MediaSourceDto {
        reference: media_source_ref.clone(),
        kind: MediaSourceKindDto::Microphone,
        media_kind: Some(MediaKindDto::Audio),
        label: String::new(),
        usages: Vec::new(),
        supported_operations: Vec::new(),
        is_default: false,
    };
    let _media_capture_options = MediaCaptureOptionsDto {
        max_duration_ms: None,
        max_dimension_px: None,
        jpeg_quality: None,
    };
    let _media_record_options = MediaRecordOptionsDto {
        max_duration_ms: None,
        mime_type: None,
    };
    let _media_transcription_inputs = [
        MediaTranscriptionInputDto::LiveSource {
            source: media_source_ref.clone(),
        },
        MediaTranscriptionInputDto::StoredMedia {
            media_ref: "asset".into(),
        },
    ];
    let _media_modes = [
        MediaSessionModeDto::Audio,
        MediaSessionModeDto::Video,
        MediaSessionModeDto::AudioVideo,
    ];
    let _media_intents = [
        MediaSessionIntentDto::Observation,
        MediaSessionIntentDto::Conversation,
    ];
    let _media_updates = [
        MediaSessionUpdateDto::SetEnabled {
            kind: MediaKindDto::Audio,
            enabled: true,
        },
        MediaSessionUpdateDto::SelectSource {
            kind: MediaKindDto::Audio,
            source: media_source_ref.clone(),
        },
        MediaSessionUpdateDto::SetPlaybackGain { gain: 1.0 },
    ];
    let _media_states = [
        MediaSessionStateDto::Preparing,
        MediaSessionStateDto::Ready,
        MediaSessionStateDto::Reconnecting,
        MediaSessionStateDto::Ended,
        MediaSessionStateDto::Failed,
    ];
    let _media_operation_request = MediaOperationRequestDto {
        identity: media_operation_id.clone(),
        owner: media_owner.clone(),
        initiator: Some(MediaInitiatorDto {
            agent_id: None,
            tool_use_id: None,
            request_id: None,
        }),
        timeout_budget_ms: None,
        max_payload_bytes: 1024,
        operation: MediaOperationDto::Sources {
            kind: None,
            usage: None,
        },
    };
    let _media_operations = vec![
        MediaOperationDto::Sources {
            kind: None,
            usage: None,
        },
        MediaOperationDto::Status {
            target: Some(media_target.clone()),
        },
        MediaOperationDto::Capture {
            source: media_source_ref.clone(),
            options: MediaCaptureOptionsDto {
                max_duration_ms: None,
                max_dimension_px: None,
                jpeg_quality: None,
            },
        },
        MediaOperationDto::Record {
            source: media_source_ref.clone(),
            options: MediaRecordOptionsDto {
                max_duration_ms: None,
                mime_type: None,
            },
        },
        MediaOperationDto::Transcribe {
            input: MediaTranscriptionInputDto::LiveSource {
                source: media_source_ref.clone(),
            },
            language: None,
        },
        MediaOperationDto::Synthesize {
            text: String::new(),
            language: None,
            rate: None,
            voice: None,
        },
        MediaOperationDto::Play {
            media_ref: "asset".into(),
            source: None,
        },
        MediaOperationDto::Speak {
            text: String::new(),
            language: None,
            rate: None,
            voice: None,
        },
        MediaOperationDto::SessionStart {
            request: media_session_request.clone(),
        },
        MediaOperationDto::SessionUpdate {
            session_id: "session".into(),
            update: MediaSessionUpdateDto::SetEnabled {
                kind: MediaKindDto::Audio,
                enabled: true,
            },
        },
        MediaOperationDto::Observe {
            target: media_target.clone(),
            after_sequence: None,
            limit: 1,
        },
        MediaOperationDto::Wait {
            target: media_target.clone(),
            after_sequence: None,
            timeout_ms: 1,
            limit: 1,
        },
        MediaOperationDto::Stop {
            target: media_target.clone(),
        },
    ];
    let _media_results = vec![
        MediaResultDto::Sources {
            sources: Vec::new(),
        },
        MediaResultDto::Status {
            resources: Vec::new(),
        },
        MediaResultDto::Captured {
            asset: media_asset.clone(),
        },
        MediaResultDto::RecordingStarted {
            recording_id: String::new(),
        },
        MediaResultDto::RecordingStopped {
            recording_id: String::new(),
            asset: media_asset.clone(),
        },
        MediaResultDto::Transcript {
            text: String::new(),
            language: None,
            confidence: None,
        },
        MediaResultDto::Synthesized {
            asset: media_asset.clone(),
        },
        MediaResultDto::PlaybackCompleted { duration_ms: 0 },
        MediaResultDto::SpeechCompleted { duration_ms: 0 },
        MediaResultDto::SessionStarted {
            session_id: String::new(),
            status: media_session_status.clone(),
        },
        MediaResultDto::SessionUpdated {
            session_id: String::new(),
            status: media_session_status.clone(),
        },
        MediaResultDto::Events {
            batch: media_event_batch.clone(),
        },
        MediaResultDto::Stopped {
            resource: media_target.clone(),
        },
    ];
    let _media_service_actions = [
        MediaServiceActionDto::Execute {
            operation: MediaOperationDto::Sources {
                kind: None,
                usage: None,
            },
            timeout_budget_ms: None,
            max_payload_bytes: None,
        },
        MediaServiceActionDto::ReadMedia {
            media_ref: String::new(),
        },
        MediaServiceActionDto::Cancel {
            operation_id: media_operation_id.clone(),
        },
    ];
    let _media_service_request = MediaServiceRequestDto {
        request_id: String::new(),
        action: MediaServiceActionDto::ReadMedia {
            media_ref: String::new(),
        },
    };
    let _media_service_results = [
        MediaServiceResultDto::Operation {
            operation_id: media_operation_id.clone(),
            result: MediaResultDto::Stopped {
                resource: media_target.clone(),
            },
        },
        MediaServiceResultDto::Content {
            content: MediaContentDto {
                media_ref: String::new(),
                metadata: media_metadata.clone(),
                bytes: Vec::new(),
            },
        },
        MediaServiceResultDto::Cancelled,
    ];
    let _media_service_response = MediaServiceResponseDto {
        request_id: String::new(),
        result: Some(MediaServiceResultDto::Cancelled),
        error: None,
    };
    let _media_resource_states = [
        MediaResourceStateDto::Ready,
        MediaResourceStateDto::Active,
        MediaResourceStateDto::Stopping,
        MediaResourceStateDto::Completed,
        MediaResourceStateDto::Failed,
    ];
    let _media_resource_status = MediaResourceStatusDto {
        resource: media_target.clone(),
        state: MediaResourceStateDto::Ready,
        started_at_unix_ms: None,
        updated_at_unix_ms: None,
    };
    let _media_event_kinds = [
        MediaEventKindDto::StateChanged {
            resource: media_target.clone(),
            state: MediaResourceStateDto::Ready,
        },
        MediaEventKindDto::Frame {
            source: media_source_ref.clone(),
            asset: media_asset.clone(),
        },
        MediaEventKindDto::Transcript {
            text: String::new(),
            language: None,
            confidence: None,
        },
        MediaEventKindDto::Error {
            kind: MediaErrorKindDto::Unavailable,
            message: String::new(),
        },
    ];
    let _media_content = MediaContentDto {
        media_ref: String::new(),
        metadata: media_metadata.clone(),
        bytes: Vec::new(),
    };
    let _media_error_kinds = [
        MediaErrorKindDto::PermissionDenied,
        MediaErrorKindDto::Busy,
        MediaErrorKindDto::Cancelled,
        MediaErrorKindDto::Timeout,
        MediaErrorKindDto::NoData,
        MediaErrorKindDto::NotFound,
        MediaErrorKindDto::Unavailable,
        MediaErrorKindDto::Unsupported,
        MediaErrorKindDto::ModelMissing,
        MediaErrorKindDto::InvalidRequest,
        MediaErrorKindDto::ProcessingFailed,
        MediaErrorKindDto::NativeFailure,
        MediaErrorKindDto::StoreFailure,
        MediaErrorKindDto::MediaTooLarge,
    ];
    let _media_errors = media_error.clone();
    let _media_pcm = MediaPcmFrameDto {
        sample_rate_hz: 24_000,
        channels: 1,
        pcm16le: Vec::new(),
    };
    let _media_video_frames = [
        MediaVideoFrameDto::Rgba8 {
            width_px: 1,
            height_px: 1,
            stride_bytes: 4,
            bytes: Vec::new(),
        },
        MediaVideoFrameDto::I420 {
            width_px: 1,
            height_px: 1,
            y_stride_bytes: 1,
            u_stride_bytes: 1,
            v_stride_bytes: 1,
            y: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
        },
    ];
    let _media_frame_payloads = [
        MediaFramePayloadDto::Audio(MediaPcmFrameDto {
            sample_rate_hz: 24_000,
            channels: 1,
            pcm16le: Vec::new(),
        }),
        MediaFramePayloadDto::Video(MediaVideoFrameDto::Rgba8 {
            width_px: 1,
            height_px: 1,
            stride_bytes: 4,
            bytes: Vec::new(),
        }),
    ];
    let _media_frame = MediaFrameDto {
        source: media_source_ref.clone(),
        sequence: 0,
        captured_at_unix_ms: 0,
        payload: MediaFramePayloadDto::Audio(MediaPcmFrameDto {
            sample_rate_hz: 24_000,
            channels: 1,
            pcm16le: Vec::new(),
        }),
    };
    let _media_receipt = MediaRenderReceiptDto {
        accepted_sample_frames: 0,
        played_sample_frames: 0,
        rendered_at_unix_ms: None,
    };
    let _media_device_update = MediaDeviceUpdateRequestDto {
        lease_id: String::new(),
        update: MediaSessionUpdateDto::SetEnabled {
            kind: MediaKindDto::Audio,
            enabled: false,
        },
    };
    let _media_device_audio_write = MediaDeviceAudioWriteRequestDto {
        lease_id: String::new(),
        frame: MediaPcmFrameDto {
            sample_rate_hz: 24_000,
            channels: 1,
            pcm16le: Vec::new(),
        },
    };
    let _media_device_video_write = MediaDeviceVideoWriteRequestDto {
        lease_id: String::new(),
        frame: MediaVideoFrameDto::Rgba8 {
            width_px: 1,
            height_px: 1,
            stride_bytes: 4,
            bytes: Vec::new(),
        },
    };
    let _media_device_audio_control = MediaDeviceAudioControlRequestDto {
        lease_id: String::new(),
    };
    let _media_device_requests = [
        MediaDeviceRequestDto::Open {
            request: MediaDeviceOpenRequestDto {
                identity: media_operation_id.clone(),
                owner: media_owner.clone(),
                initiator: None,
                timeout_budget_ms: None,
                max_payload_bytes: 0,
                request: media_session_request.clone(),
            },
        },
        MediaDeviceRequestDto::NextInput {
            lease_id: String::new(),
        },
        MediaDeviceRequestDto::WriteAudio {
            request: MediaDeviceAudioWriteRequestDto {
                lease_id: String::new(),
                frame: MediaPcmFrameDto {
                    sample_rate_hz: 1,
                    channels: 1,
                    pcm16le: Vec::new(),
                },
            },
        },
        MediaDeviceRequestDto::WriteVideo {
            request: MediaDeviceVideoWriteRequestDto {
                lease_id: String::new(),
                frame: MediaVideoFrameDto::Rgba8 {
                    width_px: 1,
                    height_px: 1,
                    stride_bytes: 4,
                    bytes: Vec::new(),
                },
            },
        },
        MediaDeviceRequestDto::FlushAudio {
            request: MediaDeviceAudioControlRequestDto {
                lease_id: String::new(),
            },
        },
        MediaDeviceRequestDto::InterruptAudio {
            request: MediaDeviceAudioControlRequestDto {
                lease_id: String::new(),
            },
        },
        MediaDeviceRequestDto::Update {
            request: MediaDeviceUpdateRequestDto {
                lease_id: String::new(),
                update: MediaSessionUpdateDto::SetEnabled {
                    kind: MediaKindDto::Audio,
                    enabled: true,
                },
            },
        },
        MediaDeviceRequestDto::Stop {
            lease_id: String::new(),
        },
    ];
    let _media_device_request = MediaDeviceRequestEnvelopeDto {
        request_id: String::new(),
        request: MediaDeviceRequestDto::Stop {
            lease_id: String::new(),
        },
    };
    let _media_device_results = [
        MediaDeviceResultDto::Opened {
            lease: MediaDeviceLeaseDto {
                lease_id: String::new(),
            },
        },
        MediaDeviceResultDto::Input { frame: None },
        MediaDeviceResultDto::AudioWritten {
            receipt: MediaRenderReceiptDto {
                accepted_sample_frames: 0,
                played_sample_frames: 0,
                rendered_at_unix_ms: None,
            },
        },
        MediaDeviceResultDto::AudioFlushed {
            receipt: MediaRenderReceiptDto {
                accepted_sample_frames: 0,
                played_sample_frames: 0,
                rendered_at_unix_ms: None,
            },
        },
        MediaDeviceResultDto::AudioInterrupted {
            receipt: MediaRenderReceiptDto {
                accepted_sample_frames: 0,
                played_sample_frames: 0,
                rendered_at_unix_ms: None,
            },
        },
        MediaDeviceResultDto::VideoWritten,
        MediaDeviceResultDto::Updated {
            status: media_session_status.clone(),
        },
        MediaDeviceResultDto::Stopped,
        MediaDeviceResultDto::Error {
            error: media_error.clone(),
        },
    ];
    let _media_device_response = MediaDeviceResponseDto {
        request_id: String::new(),
        result: MediaDeviceResultDto::Stopped,
    };
    let _media_description_types = [
        MediaSessionDescriptionTypeDto::Offer,
        MediaSessionDescriptionTypeDto::Answer,
    ];
    let _media_description = MediaSessionDescriptionDto {
        kind: MediaSessionDescriptionTypeDto::Offer,
        sdp: String::new(),
    };
    let _media_connection_request = MediaConnectionRequestDto {
        request_id: String::new(),
        connection_id: String::new(),
        session_id: String::new(),
        media_session_ref: media_session_ref.clone(),
        operation_id: media_operation_id.clone(),
        owner: media_owner.clone(),
        mode: MediaSessionModeDto::Audio,
        intent: MediaSessionIntentDto::Observation,
        audio_source: None,
        video_source: None,
        description: MediaSessionDescriptionDto {
            kind: MediaSessionDescriptionTypeDto::Offer,
            sdp: String::new(),
        },
    };
    let _media_connection_response = MediaConnectionResponseDto {
        request_id: String::new(),
        connection_id: String::new(),
        media_session_ref: media_session_ref.clone(),
        description: Some(MediaSessionDescriptionDto {
            kind: MediaSessionDescriptionTypeDto::Answer,
            sdp: String::new(),
        }),
        error: None,
    };
    let _media_control_actions = [
        MediaControlActionDto::FlushAudio {
            target_sequence: 0,
            target_sample_frames: 0,
        },
        MediaControlActionDto::InterruptAudio {
            target_sequence: 0,
            target_sample_frames: 0,
        },
    ];
    let _media_control_request = MediaControlRequestDto {
        request_id: String::new(),
        connection_id: String::new(),
        media_session_ref: media_session_ref.clone(),
        generation: 1,
        action: MediaControlActionDto::FlushAudio {
            target_sequence: 0,
            target_sample_frames: 0,
        },
    };
    let _media_control_response = MediaControlResponseDto {
        request_id: String::new(),
        connection_id: String::new(),
        media_session_ref,
        generation: 1,
        receipt: None,
        error: Some(media_error),
    };
    let _mcp_scope = WritableScopeDto::User;
    let _settings_destination = WritableScopeDto::User;
    let _permission_behavior = PermissionBehaviorDto::Allow;
    let _recovery_state = TurnRecoveryStateDto::PausedRecoverable;
    let _mode = PromptModeDto::Normal;
    let _img = ImageRefDto {
        media_type: String::new(),
        base64: String::new(),
    };
    let _cmd_result = CommandResultDto {
        display: String::new(),
        injected: None,
    };
    let _lk = ListingKindDto::Mcp;
    let _err = ClientError::Internal {
        message: String::new(),
    };
    let _not_admitted = ClientError::SubmissionNotAdmitted {
        message: String::new(),
    };
    let _msg = MessageDto {
        transcript_uuid: None,
        sent_at_ms: None,
        loop_wakeup: None,
        visualization_context: Some(client::protocol::message::VisualizationContextDto {
            id: String::new(),
            revision: 1,
            title: String::new(),
        }),
        role: String::new(),
        blocks: vec![
            MessageBlockDto::Text {
                text: String::new().into(),
            },
            MessageBlockDto::Visualization { reference: None },
        ],
        images: Vec::new(),
    };
    let _statuses = [
        client::protocol::message::VisualizationBlockStatusDto::Pending,
        client::protocol::message::VisualizationBlockStatusDto::Ready,
        client::protocol::message::VisualizationBlockStatusDto::Unavailable,
        client::protocol::message::VisualizationBlockStatusDto::Discarded,
    ];
    let _req = PermissionRequest {
        request_id: 0,
        kind: PermissionKindDto::BypassPermissionsMode,
        worker: Some(WorkerInfoDto {
            name: String::new(),
            color: String::new(),
            team: None,
        }),
        owner: None,
        suppress_always_allow_rule: false,
        auto_mode_prompt: None,
    };
    let _resolved = PermissionResolved {
        request_id: 0,
        response: PermissionResponseDto::Deny,
    };
    let _computer_access = ComputerAccessRequestDto {
        request_id: 0,
        reason: String::new(),
        apps: vec![RequestedAppDto {
            label: String::new(),
        }],
        tier: AccessTierDto::Full,
        clipboard_read: false,
        clipboard_write: false,
        system_key_combos: false,
        tcc_state: Some(TccStateDto {
            accessibility: false,
            screen_recording: false,
        }),
    };
    let _rows = (
        SessionRowDto {
            uuid: String::new(),
            title: String::new(),
            modified_rfc3339: String::new(),
            message_count: 0,
            mode: client::protocol::listings::SessionModeDto::Code,
            path: String::new(),
            workspace_path: None,
        },
        McpServerDto {
            name: String::new(),
            status: McpStatusDto::Connected,
            transport: String::new(),
        },
        SkillDto {
            name: String::new(),
            source_dir: String::new(),
        },
        HookDto {
            name: String::new(),
            event: String::new(),
            matcher: None,
            timeout_ms: 0,
            hook_type: None,
            source: None,
            content: None,
            status_message: None,
            blocking: None,
            is_async: None,
            priority: None,
            async_rewake: None,
            async_timeout_ms: None,
            if_condition: None,
        },
        AgentDto {
            name: String::new(),
            description: String::new(),
            tools_allowed: vec![],
        },
        SessionAgentSummaryDto {
            agent_id: String::new(),
            name: String::new().into(),
            agent_type: String::new(),
            model: None,
            model_profile: None,
            status: String::new(),
            latest_activity: None,
            updated_at_ms: None,
        },
        SessionAgentMessageRowDto {
            message_index: 0,
            message_uuid: String::new(),
            message: MessageDto {
                transcript_uuid: None,
                sent_at_ms: None,
                role: String::new(),
                blocks: vec![],
                images: vec![],
                loop_wakeup: None,
                visualization_context: None,
            },
            api_error_json: None,
        },
        SlashCommandDto {
            name: String::new().into(),
            description: String::new().into(),
            source: String::new(),
            aliases: vec![],
            argument_hint: None,
            argument_names: Vec::new(),
            menu_description: None,
            hidden: false,
        },
        MemoryEntryDto {
            path: String::new(),
            tier: MemoryTierDto::User,
            body: String::new(),
            age_days: 0,
            size_bytes: 0,
        },
        StatusSnapshotDto {
            session_id: String::new(),
            model: String::new(),
            model_source: None,
            n_messages: 0,
            total_cost_usd: 0.0,
            input_tokens: 0,
            output_tokens: 0,
            n_mcp_connected: 0,
            n_mcp_total: 0,
            n_hooks: 0,
            n_agents: 0,
            started_at: String::new(),
            cwd: String::new(),
            status_line: None,
            active_workers: None,
        },
        AuthStateDto::SignedOut,
        DoctorReportDto {
            checks: vec![DoctorCheckDto {
                name: String::new(),
                status: CheckStatusDto::Pass,
                detail: None,
            }],
            summary: DoctorSummaryDto {
                passed: 0,
                warnings: 0,
                failed: 0,
            },
        },
        TaskRowDto {
            agent_id: None,
            unread: false,
            model: None,
            effort: None,
            kind: None,
            awaiting_plan_approval: false,
            task_id: String::new(),
            task_type: String::new(),
            status: TaskStatusDto::Pending,
            description: client::protocol::text::ClientTextDto::default(),
            can_resume: false,
            started_at_ms: None,
            error: None,
            stage: None,
        },
        CoordinatorWorkerDto {
            agent_id: String::new(),
            name: String::new(),
            agent_type: String::new(),
            status: String::new(),
        },
    );

    // Sanity: the index is non-empty and contains a known anchor key.
    let ix = current_contract_index();
    assert!(
        ix.contains_key("ClientEvent::TextDelta.text"),
        "the contract index must enumerate the contract leaves"
    );
}
