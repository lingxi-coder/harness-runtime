//! Hook event payload (over-the-wire JSON) and response parser.
//!
//! Byte-locked against `claude-code/src/entrypoints/sdk/coreSchemas.ts:414-446`
//! (`PreToolUseHookInputSchema` / `PostToolUseHookInputSchema`) and
//! `claude-code/src/utils/hooks.ts:540-680` (response-processing block).

#![forbid(unsafe_code)]

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde::de::{Deserializer, Error as DeError};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::response::{
    ExactHookText, HookDecision, HookJson, HookResponse, PermissionRequestResult,
};

/// Marker unit struct that serializes/deserializes as the literal `"PreToolUse"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookEventNamePre;

impl Serialize for HookEventNamePre {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("PreToolUse")
    }
}
impl<'de> Deserialize<'de> for HookEventNamePre {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "PreToolUse" {
            Ok(Self)
        } else {
            Err(D::Error::custom(format!(
                "expected 'PreToolUse', got {s:?}"
            )))
        }
    }
}

/// Marker unit struct that serializes/deserializes as the literal `"PostToolUse"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookEventNamePost;

impl Serialize for HookEventNamePost {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("PostToolUse")
    }
}
impl<'de> Deserialize<'de> for HookEventNamePost {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "PostToolUse" {
            Ok(Self)
        } else {
            Err(D::Error::custom(format!(
                "expected 'PostToolUse', got {s:?}"
            )))
        }
    }
}

/// Generate a marker unit struct that serializes/deserializes as a single
/// literal hook-event-name string. Mirrors the [`HookEventNamePre`] /
/// [`HookEventNamePost`] hand-written markers for the lifecycle events.
macro_rules! hook_event_name_marker {
    ($ty:ident, $lit:literal) => {
        #[doc = concat!("Marker unit struct that serializes/deserializes as the literal `\"", $lit, "\"`.")]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $ty;

        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str($lit)
            }
        }
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                if s == $lit {
                    Ok(Self)
                } else {
                    Err(D::Error::custom(format!(
                        concat!("expected '", $lit, "', got {:?}"),
                        s
                    )))
                }
            }
        }
    };
}

hook_event_name_marker!(HookEventNameStop, "Stop");
hook_event_name_marker!(HookEventNameSubagentStop, "SubagentStop");
hook_event_name_marker!(HookEventNameTaskCompleted, "TaskCompleted");
hook_event_name_marker!(HookEventNameTaskCreated, "TaskCreated");
hook_event_name_marker!(HookEventNameUserPromptSubmit, "UserPromptSubmit");
hook_event_name_marker!(HookEventNameSessionStart, "SessionStart");
hook_event_name_marker!(HookEventNameStopFailure, "StopFailure");
// B6 — additional lifecycle / environment events whose `HookEvent` variant
// already exists. Each marker serializes to exactly its wire literal.
hook_event_name_marker!(HookEventNamePostToolUseFailure, "PostToolUseFailure");
hook_event_name_marker!(HookEventNameSessionEnd, "SessionEnd");
hook_event_name_marker!(HookEventNamePreCompact, "PreCompact");
hook_event_name_marker!(HookEventNamePostCompact, "PostCompact");
hook_event_name_marker!(HookEventNamePreModelSwitch, "PreModelSwitch");
hook_event_name_marker!(HookEventNamePostModelSwitch, "PostModelSwitch");
hook_event_name_marker!(HookEventNameNotification, "Notification");
hook_event_name_marker!(HookEventNamePermissionRequest, "PermissionRequest");
hook_event_name_marker!(HookEventNamePermissionDenied, "PermissionDenied");
hook_event_name_marker!(HookEventNameSetup, "Setup");
hook_event_name_marker!(HookEventNameSubagentStart, "SubagentStart");
hook_event_name_marker!(HookEventNameAgentSpawn, "AgentSpawn");
hook_event_name_marker!(HookEventNameCwdChanged, "CwdChanged");
hook_event_name_marker!(HookEventNameDirectoryAdded, "DirectoryAdded");
hook_event_name_marker!(HookEventNameFileChanged, "FileChanged");
hook_event_name_marker!(HookEventNameWorktreeRemove, "WorktreeRemove");
// Deferred-completion batch — the final four events whose `HookEvent` variant
// previously lacked a field to source a *required* wire value. Each marker
// serializes to exactly its wire literal.
hook_event_name_marker!(HookEventNameConfigChange, "ConfigChange");
hook_event_name_marker!(HookEventNameInstructionsLoaded, "InstructionsLoaded");
hook_event_name_marker!(HookEventNameElicitation, "Elicitation");
hook_event_name_marker!(HookEventNameWorktreeCreate, "WorktreeCreate");
hook_event_name_marker!(HookEventNameTeammateIdle, "TeammateIdle");
// #39 — three more user-configurable events.
hook_event_name_marker!(HookEventNamePostToolBatch, "PostToolBatch");
hook_event_name_marker!(HookEventNameUserPromptExpansion, "UserPromptExpansion");
hook_event_name_marker!(HookEventNameMessageDisplay, "MessageDisplay");
// ElicitationResult — fires after the user responds to an MCP elicitation
// (binary-confirmed at BIN off ~201751493; key literal `"ElicitationResult"`).
hook_event_name_marker!(HookEventNameElicitationResult, "ElicitationResult");

/// Wire-format `effort` object embedded in the base hook input shape
/// (1:1 with `coreSchemas.ts` base `RT` schema:
/// `effort: E.object({ level: E.string() }).optional()`).
///
/// claude-code construction (`createBaseHookInput`, minified `vd`):
/// `effort: s && getAppState && Lw(s) ? { level: jO(s, i) } : void 0` — i.e. the
/// object is present (`{ level }`) ONLY for hooks that fire within a tool-use
/// context on a model that supports the effort parameter, and omitted entirely
/// (the optional spread collapses) for session-lifecycle hooks and models
/// without effort support. `level` is the active effort level for the turn
/// (`"low"` / `"medium"` / `"high"` / `"xhigh"` / `"max"`), after any silent
/// downgrade for the selected model — the same value exposed to hook commands
/// and Bash as the `LINGXI_EFFORT` env var. Same shape as
/// `StatusLineCommandInput.effort`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct EffortLevel {
    pub level: String,
}

impl EffortLevel {
    /// Construct an `effort` object from the active effort `level` string.
    #[must_use]
    pub fn new(level: impl Into<String>) -> Self {
        Self {
            level: level.into(),
        }
    }
}

/// One entry in a `Stop` / `SubagentStop` payload `background_tasks` array
/// (1:1 with claude-code `Lic`). `id` / `type` / `status` / `description`
/// are always present in that order; the per-task-type extras are appended
/// after `description` (claude switches on `n.type`):
/// `local_bash` → `command`; `local_agent` → `agent_type`;
/// `monitor_mcp` / `mcp_task` → `server`, `tool`; `local_workflow` → `name`.
/// Each task type sets a disjoint subset, so declaration order
/// (`command < agent_type < server < tool < name`) reproduces every
/// claude case byte-exactly. The wire `type` value is claude's
/// kind-label map `O1o` (e.g. `local_bash` → `"shell"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct HookBackgroundTask {
    /// Internal goal-deferral metadata; never part of the hook payload wire.
    #[serde(skip)]
    pub is_idle: bool,
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub status: String,
    #[serde(with = "crate::definition::hook_if_pattern_source")]
    pub description: Utf16JsonProjection,
    /// `local_bash` only.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::definition::hook_if_pattern_source::serialize_option",
        deserialize_with = "crate::definition::hook_if_pattern_source::deserialize_option"
    )]
    pub command: Option<Utf16JsonProjection>,
    /// `local_agent` only.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    /// `monitor_mcp` / `mcp_task`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub server: Option<String>,
    /// `monitor_mcp` / `mcp_task`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool: Option<String>,
    /// `local_workflow` only.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
}

/// One entry in a `Stop` / `SubagentStop` payload `session_crons` array
/// (1:1 with claude-code `Mic`): `{id, schedule, recurring, prompt}` in
/// that order. `schedule` ← claude `t.cron`; `recurring` ← `t.recurring ?? false`;
/// `prompt` is truncated to 1000 chars upstream (claude `TUe(t.prompt, 1000)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct HookSessionCron {
    pub id: String,
    pub schedule: String,
    pub recurring: bool,
    #[serde(with = "crate::definition::hook_if_pattern_source")]
    pub prompt: Utf16JsonProjection,
}

/// Wire-format `PreToolUse` payload (1:1 with `coreSchemas.ts:414-423`).
///
/// Field meanings track claude-code exactly; see the schema reference above.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct PreToolUsePayload {
    pub hook_event_name: HookEventNamePre,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// `prompt_id` — UUID correlating a user prompt with all subsequent
    /// events until the next prompt. Oracle `createBaseHookInput`
    /// (2.1.238 minified `c_`, BIN off 296935693) emits it between `cwd`
    /// and `permission_mode`: `prompt_id:Vut()??void 0`. Absent until the
    /// first user input of the process lifetime, so `None` omits the key.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub tool_name: String,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub tool_input: HookJson,
    pub tool_use_id: String,
}

/// Wire-format `PostToolUse` payload (1:1 with `coreSchemas.ts:436-446`).
///
/// Field meanings track claude-code exactly; see the schema reference above.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct PostToolUsePayload {
    pub hook_event_name: HookEventNamePost,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub tool_name: String,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub tool_input: HookJson,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub tool_response: HookJson,
    pub tool_use_id: String,
    /// Tool execution time in ms (binary appends `duration_ms` last; schema
    /// `.number().optional()` — "Tool execution time in milliseconds. Excludes
    /// permission-prompt and hook time.").
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub duration_ms: Option<u64>,
}

/// Wire-format `Stop` payload (1:1 with `coreSchemas.ts:513-527`
/// `StopHookInputSchema`; constructed at `utils/hooks.ts:3680-3684`).
///
/// Field order mirrors the existing tool payloads: `hook_event_name` first,
/// then the `createBaseHookInput` base fields, then the event-specific fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct StopPayload {
    pub hook_event_name: HookEventNameStop,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub stop_hook_active: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_assistant_message: Option<String>,
    /// Background-task snapshot, spread LAST by claude (`...m`, after
    /// `last_assistant_message`) when a tool-use context is present. `None`
    /// (claude `m = void 0`) omits both this and `session_crons`; `Some([])`
    /// emits `[]`. 1:1 with claude `Lic(taskRegistry.all())`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub background_tasks: Option<Vec<HookBackgroundTask>>,
    /// Session-cron snapshot, spread immediately after `background_tasks`
    /// (1:1 with claude `Mic()`). See [`Self::background_tasks`] for the
    /// None-vs-empty semantics.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_crons: Option<Vec<HookSessionCron>>,
}

/// Wire-format `SubagentStop` payload (1:1 with `coreSchemas.ts:550-567`
/// `SubagentStopHookInputSchema`; constructed at `utils/hooks.ts:3671-3678`).
///
/// `agent_id` / `agent_type` are **required** here (vs. the optional base
/// fields) and `agent_transcript_path` is subagent-specific.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct SubagentStopPayload {
    pub hook_event_name: HookEventNameSubagentStop,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    pub stop_hook_active: bool,
    pub agent_id: String,
    pub agent_transcript_path: String,
    pub agent_type: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_assistant_message: Option<String>,
    /// Background-task snapshot — see [`StopPayload::background_tasks`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub background_tasks: Option<Vec<HookBackgroundTask>>,
    /// Session-cron snapshot — see [`StopPayload::session_crons`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_crons: Option<Vec<HookSessionCron>>,
}

/// Wire-format `TaskCompleted` payload (1:1 with `coreSchemas.ts:614-625`
/// `TaskCompletedHookInputSchema`; constructed at `utils/hooks.ts:3799-3807`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct TaskCompletedPayload {
    pub hook_event_name: HookEventNameTaskCompleted,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub task_id: String,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub task_subject: Utf16JsonProjection,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option"
    )]
    pub task_description: Option<Utf16JsonProjection>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub teammate_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub team_name: Option<String>,
}

/// Wire-format `TaskCreated` payload (1:1 with `coreSchemas.ts:601-612`
/// `TaskCreatedHookInputSchema`; constructed at `utils/hooks.ts:3756-3764`).
///
/// Field set mirrors `TaskCompletedPayload` exactly (`task_id` / `task_subject`
/// required, `task_description` / `teammate_name` / `team_name` optional) — the
/// two schemas differ only in their `hook_event_name` literal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct TaskCreatedPayload {
    pub hook_event_name: HookEventNameTaskCreated,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub task_id: String,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub task_subject: Utf16JsonProjection,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option"
    )]
    pub task_description: Option<Utf16JsonProjection>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub teammate_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub team_name: Option<String>,
}

/// Wire-format `TeammateIdle` payload (1:1 with `coreSchemas.ts:591-598`
/// `TeammateIdleHookInputSchema`; constructed at `utils/hooks.ts:3716-3720`).
///
/// Fired when a teammate's query loop stops and it is about to park awaiting the
/// next message (claude-code `stopHooks.ts:403`, gated on `isTeammate()`).
/// `teammate_name` / `team_name` are BOTH required strings in the TS schema
/// (unlike the `optional` pair on `TaskCompleted` / `TaskCreated`), so they are
/// non-`Option` here. `team_name` may be `""` when the firing scope cannot reach
/// the team identity (same documented leaf-scope gap as the `TaskCompleted`
/// `team_name: None`) — TS itself falls back to `getTeamName() ?? ''`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct TeammateIdlePayload {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// Oracle 2.1.263 Sa: present only when the scratchpad feature has a path.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub scratchpad_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    // E_n spreads Sa before adding the event marker. It does not pass a tool
    // context to Sa, so agent_id and effort are absent for this event.
    pub hook_event_name: HookEventNameTeammateIdle,
    pub teammate_name: String,
    pub team_name: String,
}

/// Wire-format `UserPromptSubmit` payload (1:1 with `coreSchemas.ts:484-491`
/// `UserPromptSubmitHookInputSchema`; constructed at `utils/hooks.ts:3840-3843`).
///
/// `session_title` is optional (binary-confirmed at BIN off 201745825:
/// `{hook_event_name:"UserPromptSubmit", prompt:string, session_title?:string}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct UserPromptSubmitPayload {
    pub hook_event_name: HookEventNameUserPromptSubmit,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub prompt: Utf16JsonProjection,
    /// Current session title at the time the prompt is submitted (optional,
    /// binary-confirmed `session_title` key at BIN off 201745825).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_title: Option<String>,
}

/// Wire-format `SessionStart` payload (1:1 with `coreSchemas.ts:493-502`
/// `SessionStartHookInputSchema`; constructed at `utils/hooks.ts:3876-3881`).
///
/// `source` is one of `startup` / `resume` / `clear` / `compact` in TS;
/// modelled here as a free `String` (validation happens upstream).
/// `session_title` is optional (binary-confirmed at BIN off 201745825:
/// `{…, source:enum, agent_type?:string, model?:string, session_title?:string}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct SessionStartPayload {
    pub hook_event_name: HookEventNameSessionStart,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    /// Current session title at session start (optional,
    /// binary-confirmed `session_title` key at BIN off ~201746000).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_title: Option<String>,
}

/// Wire-format `StopFailure` payload (1:1 with `coreSchemas.ts:529-538`
/// `StopFailureHookInputSchema`; constructed at `utils/hooks.ts:3613-3619`).
///
/// `error` is one of the `SDKAssistantMessageError` enum members
/// (`coreSchemas.ts:1256-1266`); modelled here as a free `String`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct StopFailurePayload {
    pub hook_event_name: HookEventNameStopFailure,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error_details: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_assistant_message: Option<String>,
}

/// Wire-format `PostToolUseFailure` payload (1:1 with `coreSchemas.ts:448-459`
/// `PostToolUseFailureHookInputSchema`; constructed at `utils/hooks.ts:3509-3517`).
///
/// The `HookEvent::PostToolUseFailure` variant carries the dispatched
/// `tool_input` (threaded from the turn loop's `effective_input`, matching the
/// `PostToolUse` success arm). The `is_interrupt` flag still defaults to `None`
/// until richer context is threaded through (consistent with the B1 lifecycle
/// arms' default-fill convention).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct PostToolUseFailurePayload {
    pub hook_event_name: HookEventNamePostToolUseFailure,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub tool_name: String,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub tool_input: HookJson,
    pub tool_use_id: String,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub error: Utf16JsonProjection,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub is_interrupt: Option<bool>,
    /// Tool execution time in ms (binary appends `duration_ms` after
    /// `is_interrupt`; schema `.number().optional()`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub duration_ms: Option<u64>,
}

/// Wire-format `SessionEnd` payload (1:1 with `coreSchemas.ts:758-765`
/// `SessionEndHookInputSchema`; constructed at `utils/hooks.ts:4113-4117`).
///
/// `reason` is one of the `ExitReason` enum members (`coreSchemas.ts:747-754`);
/// modelled here as a free `String` (validation happens upstream).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct SessionEndPayload {
    pub hook_event_name: HookEventNameSessionEnd,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub reason: String,
}

/// Wire-format `PreCompact` payload (1:1 with `coreSchemas.ts:569-577`
/// `PreCompactHookInputSchema`; constructed at `utils/hooks.ts:3972-3977`).
///
/// `trigger` is `manual` / `auto` in TS; modelled here as a free `String` fed
/// from `HookEvent::PreCompact.reason`. `custom_instructions` is `.nullable()`
/// (not `.optional()`) in the schema, so it is always serialized — as JSON
/// `null` when absent — rather than skipped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct PreCompactPayload {
    pub hook_event_name: HookEventNamePreCompact,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub trigger: String,
    /// `.nullable()` in the schema — always present, `null` when absent.
    #[serde(
        default,
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub custom_instructions: Option<Utf16JsonProjection>,
}

/// Wire-format `PostCompact` payload (1:1 with `coreSchemas.ts:579-589`
/// `PostCompactHookInputSchema`; constructed at `utils/hooks.ts:4044-4049`).
///
/// `trigger` (`manual` / `auto`) is fed from the `HookEvent::PostCompact`
/// variant's `trigger` field (the compaction firing site passes `"manual"` /
/// `"auto"`). `compact_summary` is fed from the variant's `summary`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct PostCompactPayload {
    pub hook_event_name: HookEventNamePostCompact,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub trigger: String,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub compact_summary: Utf16JsonProjection,
}

/// Wire-format `PreModelSwitch` payload (Claude Code 2.1.251
/// `PreModelSwitchHookInputSchema`). Cache metadata fields are required by the
/// upstream schema, while `requested_model` is required-but-nullable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct PreModelSwitchPayload {
    pub hook_event_name: HookEventNamePreModelSwitch,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub from_model: String,
    pub to_model: String,
    pub requested_model: Option<String>,
    pub source: String,
    pub context_tokens: u64,
    pub prompt_cache_warm: bool,
    pub cache_ttl: String,
    pub estimated_cache_write_usd: f64,
    pub pricing: String,
}

/// Wire-format `PostModelSwitch` payload (Claude Code 2.1.251
/// `PostModelSwitchHookInputSchema`). Cache metadata fields are required by the
/// upstream schema, while `requested_model` is required-but-nullable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct PostModelSwitchPayload {
    pub hook_event_name: HookEventNamePostModelSwitch,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub from_model: String,
    pub to_model: String,
    pub requested_model: Option<String>,
    pub source: String,
    pub context_tokens: u64,
    pub prompt_cache_warm: bool,
    pub cache_ttl: String,
    pub estimated_cache_write_usd: f64,
    pub pricing: String,
}

/// Wire-format `Notification` payload (1:1 with `coreSchemas.ts:473-482`
/// `NotificationHookInputSchema`; constructed at `utils/hooks.ts:3579-3585`).
///
/// `notification_type` is fed from `HookEvent::Notification.kind`. `title` has
/// no field on the variant yet and defaults to `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct NotificationPayload {
    pub hook_event_name: HookEventNameNotification,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub title: Option<String>,
    pub notification_type: String,
}

/// Wire-format `PermissionRequest` payload (1:1 with `coreSchemas.ts:425-434`
/// `PermissionRequestHookInputSchema`; constructed at `utils/hooks.ts:4174-4180`).
///
/// The schema carries `tool_name`, `tool_input`, and optional
/// `permission_suggestions`. The `HookEvent::PermissionRequest.reason` field has
/// no wire counterpart (the schema has no `reason`), so it is intentionally
/// dropped. `permission_suggestions` has no engine source yet → `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct PermissionRequestPayload {
    pub hook_event_name: HookEventNamePermissionRequest,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub tool_name: String,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub tool_input: HookJson,
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub permission_suggestions: Option<HookJson>,
}

/// Wire-format `PermissionDenied` payload (1:1 with `coreSchemas.ts:461-471`
/// `PermissionDeniedHookInputSchema`; constructed at `utils/hooks.ts:3545-3552`).
///
/// Carries `tool_name`, `tool_input`, `tool_use_id`, and `reason` — all required
/// by the schema and fed directly from the `HookEvent::PermissionDenied` variant.
/// Built with `createBaseHookInput(permissionMode, undefined, toolUseContext)`
/// (`utils/hooks.ts:3546`), so it threads the engine's `permission_mode`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct PermissionDeniedPayload {
    pub hook_event_name: HookEventNamePermissionDenied,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub tool_name: String,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub tool_input: HookJson,
    pub tool_use_id: String,
    #[serde(with = "crate::definition::hook_if_pattern_source")]
    pub reason: Utf16JsonProjection,
}

/// Wire-format `Setup` payload (1:1 with `coreSchemas.ts:504-511`
/// `SetupHookInputSchema`; constructed at `utils/hooks.ts:3908-3912`).
///
/// `trigger` (`init` / `maintenance`) is fed from the `HookEvent::Setup`
/// variant's `trigger` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct SetupPayload {
    pub hook_event_name: HookEventNameSetup,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub trigger: String,
}

/// Wire-format `SubagentStart` payload (1:1 with `coreSchemas.ts:540-548`
/// `SubagentStartHookInputSchema`; constructed at `utils/hooks.ts:3938-3943`).
///
/// `agent_id` / `agent_type` are **required** here and fed directly from the
/// `HookEvent::SubagentStart` variant. `parent_agent_id` has no wire field and
/// is dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct SubagentStartPayload {
    pub hook_event_name: HookEventNameSubagentStart,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    pub agent_id: String,
    pub agent_type: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
}

/// Wire-format `AgentSpawn` payload — what a function hook inspects before a
/// subagent is spawned (claude-code `agent.spawn`).
///
/// Carries the four fields upstream lets a hook rewrite, so the hook can read
/// what it is about to change. ⛔ The spawn PROMPT is deliberately absent: it
/// can be long and carries the parent's task text, and no rewrite rule needs it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpawnPayload {
    pub hook_event_name: HookEventNameAgentSpawn,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    /// The requested subagent type.
    pub agent_type: String,
    /// The requested model, when the spawn pinned one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    /// Configured provider route for the selected child model.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model_profile: Option<String>,
    /// Whether the spawn was requested as a background run.
    pub background: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parent_agent_id: Option<String>,
}

/// Wire-format `CwdChanged` payload (1:1 with `coreSchemas.ts:727-735`
/// `CwdChangedHookInputSchema`; constructed at `utils/hooks.ts:4269-4274`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct CwdChangedPayload {
    pub hook_event_name: HookEventNameCwdChanged,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub old_cwd: String,
    pub new_cwd: String,
}

/// Wire-format `DirectoryAdded` payload (2.1.219).
///
/// claude-code `a$t` (BIN off 237753662):
/// `{...Kf(void 0),hook_event_name:"DirectoryAdded",directory:e,source:t}`,
/// dispatched with `matchQuery: t` — a hook `matcher` is therefore tested
/// against the SOURCE (`slash_command`, `register_repo_root`), not the path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryAddedPayload {
    pub hook_event_name: HookEventNameDirectoryAdded,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    /// The directory that was added.
    pub directory: String,
    /// What added it — also the matcher query.
    pub source: String,
}

/// Wire-format `FileChanged` payload (1:1 with `coreSchemas.ts:737-745`
/// `FileChangedHookInputSchema`; constructed at `utils/hooks.ts:4287-4292`).
///
/// `file_path` is fed from `HookEvent::FileChanged.path`; `event`
/// (`change` / `add` / `unlink`) from `.kind`, modelled as a free `String`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct FileChangedPayload {
    pub hook_event_name: HookEventNameFileChanged,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub file_path: String,
    pub event: String,
}

/// Wire-format `WorktreeRemove` payload (1:1 with `coreSchemas.ts:718-725`
/// `WorktreeRemoveHookInputSchema`; constructed at `utils/hooks.ts` worktree
/// removal site). `worktree_path` is fed from `HookEvent::WorktreeRemove.path`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct WorktreeRemovePayload {
    pub hook_event_name: HookEventNameWorktreeRemove,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub worktree_path: String,
}

/// Wire-format `ConfigChange` payload (1:1 with `coreSchemas.ts:670-678`
/// `ConfigChangeHookInputSchema`; constructed at `utils/hooks.ts:4219-4224`).
///
/// `source` is required (one of [`ConfigChangeSource`]); `file_path` is
/// `.optional()` so it is skipped when absent. The base shape is built with
/// `createBaseHookInput(undefined)` — no `permission_mode` is threaded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct ConfigChangePayload {
    pub hook_event_name: HookEventNameConfigChange,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub source: crate::events::ConfigChangeSource,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub file_path: Option<String>,
}

/// Wire-format `InstructionsLoaded` payload (1:1 with `coreSchemas.ts:695-707`
/// `InstructionsLoadedHookInputSchema`; constructed at
/// `utils/hooks.ts:4353-4362`).
///
/// `file_path` / `memory_type` / `load_reason` are required; `globs` /
/// `trigger_file_path` / `parent_file_path` are `.optional()` and skipped when
/// absent. Built with `createBaseHookInput(undefined)` — no `permission_mode`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct InstructionsLoadedPayload {
    pub hook_event_name: HookEventNameInstructionsLoaded,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub file_path: String,
    pub memory_type: crate::events::InstructionsMemoryType,
    pub load_reason: crate::events::InstructionsLoadReason,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub globs: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub trigger_file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parent_file_path: Option<String>,
}

/// Wire-format `Elicitation` payload (1:1 with `coreSchemas.ts:627-643`
/// `ElicitationHookInputSchema`; constructed at `utils/hooks.ts:4491-4500`).
///
/// `mcp_server_name` / `message` are required; `mode` / `url` /
/// `elicitation_id` / `requested_schema` are `.optional()` and skipped when
/// absent. Built with `createBaseHookInput(permissionMode)` — unlike the other
/// three deferred events, `permission_mode` IS threaded here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct ElicitationPayload {
    pub hook_event_name: HookEventNameElicitation,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub mcp_server_name: String,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub message: Utf16JsonProjection,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mode: Option<crate::events::ElicitationMode>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub url: Option<Utf16JsonProjection>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub elicitation_id: Option<Utf16JsonProjection>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub requested_schema: Option<HookJson>,
}

/// Wire-format `ElicitationResult` payload. 1:1 with the binary's
/// `ElicitationResultHookInputSchema` (BIN off ~201751493):
/// `gT().and({hook_event_name:"ElicitationResult", mcp_server_name:string,
/// elicitation_id?:string, mode?:enum(["form","url"]),
/// action:enum(["accept","decline","cancel"]), content?:record(string,unknown)})`.
///
/// Fired after the user responds to an MCP elicitation (or a hook intercepts it).
/// `action` is REQUIRED; `elicitation_id`, `mode`, and `content` are optional.
/// Built with `createBaseHookInput(permissionMode)` (same as `Elicitation`), so
/// `permission_mode` IS threaded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct ElicitationResultPayload {
    pub hook_event_name: HookEventNameElicitationResult,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    /// Name of the MCP server that requested elicitation (wire `mcp_server_name`,
    /// required). Sourced from `HookEvent::ElicitationResult.server_name`.
    pub mcp_server_name: String,
    /// Server-assigned elicitation ID (optional).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub elicitation_id: Option<String>,
    /// Presentation mode (`form` / `url`), if specified (optional).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mode: Option<crate::events::ElicitationMode>,
    /// User's response action (required): `"accept"` / `"decline"` / `"cancel"`.
    /// Extracted from `HookEvent::ElicitationResult.result["action"]` or defaults
    /// to `"cancel"` when the field is absent from the result JSON.
    pub action: String,
    /// Structured form content returned by the user (optional). Extracted from
    /// `HookEvent::ElicitationResult.result["content"]`.
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub content: Option<HookJson>,
}

/// Wire-format `WorktreeCreate` payload (1:1 with `coreSchemas.ts:709-716`
/// `WorktreeCreateHookInputSchema`; constructed at `utils/hooks.ts:4931-4935`).
///
/// `name` is the only event-specific field — the hook's stdout returns the
/// resolved worktree path, so the input carries just the requested `name`.
/// Built with `createBaseHookInput(undefined)` — no `permission_mode`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct WorktreeCreatePayload {
    pub hook_event_name: HookEventNameWorktreeCreate,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub name: String,
}

/// Wire-format `PostToolBatch` payload (#39). 1:1 with claude-code's input
/// schema (BIN off 200753854): the base shape `.and({hook_event_name:
/// "PostToolBatch", tool_calls:E.array(ggp())})`, where each element is a
/// [`crate::events::PostToolBatchCall`]. Fired once after every tool call in a
/// batch resolves, before the next model request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct PostToolBatchPayload {
    pub hook_event_name: HookEventNamePostToolBatch,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub tool_calls: Vec<crate::events::PostToolBatchCall>,
}

/// Wire-format `UserPromptExpansion` payload (#39). 1:1 with claude-code's input
/// schema (BIN off 200754686): the base shape `.and({hook_event_name:
/// "UserPromptExpansion", expansion_type:E.enum(["slash_command","mcp_prompt"]),
/// command_name:string, command_args:string, command_source:string().optional(),
/// prompt:string})`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct UserPromptExpansionPayload {
    pub hook_event_name: HookEventNameUserPromptExpansion,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub expansion_type: crate::events::PromptExpansionType,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub command_name: Utf16JsonProjection,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub command_args: Utf16JsonProjection,
    #[serde(
        default,
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub command_source: Option<Utf16JsonProjection>,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub prompt: Utf16JsonProjection,
}

/// Wire-format `MessageDisplay` payload (#39). 1:1 with claude-code's input
/// schema (BIN off 200761745): the base shape `.and({hook_event_name:
/// "MessageDisplay", turn_id:string, message_id:string, index:number().int(),
/// final:bool, delta:string})`. The `final` wire key is renamed from the Rust
/// keyword via serde. Fired per assistant-message flush, synchronously, with
/// per-invocation telemetry suppressed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs, reason = "wire-format mirror of claude-code schema")]
pub struct MessageDisplayPayload {
    pub hook_event_name: HookEventNameMessageDisplay,
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    /// See [`PreToolUsePayload::prompt_id`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortLevel>,
    pub turn_id: String,
    pub message_id: String,
    pub index: u64,
    #[serde(rename = "final")]
    pub is_final: bool,
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub delta: Utf16JsonProjection,
}

/// Envelope used to send one of either payload kind across the wire.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
#[allow(missing_docs, reason = "variants delegate to documented payload types")]
pub enum HookEventEnvelope {
    Pre(PreToolUsePayload),
    Post(PostToolUsePayload),
}

impl<'de> Deserialize<'de> for HookEventEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error as DeError;
        use serde_json::value::RawValue;

        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let projection = HookJson::parse(raw.get()).map_err(D::Error::custom)?;
        match projection.string_at("/hook_event_name") {
            Some("PreToolUse") => serde_json::from_str::<PreToolUsePayload>(raw.get())
                .map(Self::Pre)
                .map_err(D::Error::custom),
            Some("PostToolUse") => serde_json::from_str::<PostToolUsePayload>(raw.get())
                .map(Self::Post)
                .map_err(D::Error::custom),
            _ => Err(D::Error::custom("unknown hook event envelope")),
        }
    }
}

/// Failure modes from [`parse_response`].
#[derive(Debug, Clone, Error)]
pub enum HookResponseParseError {
    /// JSON deserialisation failed.
    #[error("hook response is not valid JSON: {0}")]
    Json(String),
    /// JSON parsed but the top level is not an object.
    #[error("hook response is not a JSON object")]
    NotObject,
    /// JSON parsed as an object, but a known hook-output field has the wrong
    /// type. Unknown fields remain intentionally compatible and are ignored.
    /// The wording follows Claude Code 2.1.251's `Hook JSON output validation
    /// failed` diagnostic closely enough for callers to surface the schema
    /// failure without treating it as a permission decision.
    #[error(
        "Hook JSON output validation failed — {field}: expected {expected}, received {actual}"
    )]
    Schema {
        /// Dotted path to the invalid known field.
        field: String,
        /// Expected JSON type.
        expected: &'static str,
        /// Actual JSON type (or `missing` for a required field).
        actual: &'static str,
    },
    /// A required field in the event-specific output object is absent.
    #[error("Hook JSON output validation failed — {field}: expected {expected}, received missing")]
    MissingSchemaField {
        /// Dotted path to the missing known field.
        field: String,
        /// Expected JSON type.
        expected: &'static str,
    },
    /// `hookSpecificOutput.hookEventName` didn't match the expected event.
    #[error("hook response hookEventName mismatch: expected '{expected}', got '{got}'")]
    EventNameMismatch {
        /// Expected event name (`"PreToolUse"` or `"PostToolUse"`).
        expected: &'static str,
        /// Actual value the hook returned.
        got: String,
    },
    /// Legacy top-level `decision` had an unrecognised value. Mirrors `azn`'s
    /// first switch `default: throw Error(`Unknown hook decision type:
    /// ${e.decision}. Valid types are: approve, block`)` (claude-code BIN off
    /// ~205721920) — the binary rejects the WHOLE hook output, so we surface a
    /// parse error rather than silently ignoring the unknown value.
    #[error("Unknown hook decision type: {value}. Valid types are: approve, block")]
    UnknownDecision {
        /// The unrecognised `decision` value the hook returned.
        value: String,
    },
    /// `hookSpecificOutput.permissionDecision` had an unrecognised value.
    /// Mirrors `azn`'s second switch `default: throw Error(`Unknown hook
    /// permissionDecision type: ${...}. Valid types are: allow, deny, ask,
    /// defer`)` (claude-code BIN off ~205721920).
    #[error(
        "Unknown hook permissionDecision type: {value}. Valid types are: allow, deny, ask, defer"
    )]
    UnknownPermissionDecision {
        /// The unrecognised `permissionDecision` value the hook returned.
        value: String,
    },
}

fn json_value_type(value: lingxi_core::types::utf16_json::JsonValueView<'_>) -> &'static str {
    if value.is_null() {
        "null"
    } else if value.as_bool().is_some() {
        "boolean"
    } else if value.as_number().is_some() {
        "number"
    } else if value.as_str().is_some() {
        "string"
    } else if value.array_len().is_some() {
        "array"
    } else {
        "object"
    }
}

fn is_json_string(value: lingxi_core::types::utf16_json::JsonValueView<'_>) -> bool {
    value.as_str().is_some()
}

fn is_json_boolean(value: lingxi_core::types::utf16_json::JsonValueView<'_>) -> bool {
    value.as_bool().is_some()
}

fn is_json_object(value: lingxi_core::types::utf16_json::JsonValueView<'_>) -> bool {
    value.object_entries().is_some()
}

fn is_json_array(value: lingxi_core::types::utf16_json::JsonValueView<'_>) -> bool {
    value.array_len().is_some()
}

fn validate_optional_field_type_at<'a>(
    object: lingxi_core::types::utf16_json::JsonValueView<'a>,
    field: &str,
    path: &str,
    expected: &'static str,
    is_expected: fn(lingxi_core::types::utf16_json::JsonValueView<'a>) -> bool,
) -> Result<(), HookResponseParseError> {
    if let Some(value) = object.get_property(field) {
        if !is_expected(value) {
            return Err(HookResponseParseError::Schema {
                field: path.to_string(),
                expected,
                actual: json_value_type(value),
            });
        }
    }
    Ok(())
}

fn validate_optional_field_type<'a>(
    object: lingxi_core::types::utf16_json::JsonValueView<'a>,
    field: &str,
    expected: &'static str,
    is_expected: fn(lingxi_core::types::utf16_json::JsonValueView<'a>) -> bool,
) -> Result<(), HookResponseParseError> {
    validate_optional_field_type_at(object, field, field, expected, is_expected)
}

fn validate_optional_nested_field_type<'a>(
    object: lingxi_core::types::utf16_json::JsonValueView<'a>,
    field: &str,
    expected: &'static str,
    is_expected: fn(lingxi_core::types::utf16_json::JsonValueView<'a>) -> bool,
) -> Result<(), HookResponseParseError> {
    let path = format!("hookSpecificOutput.{field}");
    validate_optional_field_type_at(object, field, &path, expected, is_expected)
}

fn validate_optional_permission_decision_field_type<'a>(
    object: lingxi_core::types::utf16_json::JsonValueView<'a>,
    field: &str,
    expected: &'static str,
    is_expected: fn(lingxi_core::types::utf16_json::JsonValueView<'a>) -> bool,
) -> Result<(), HookResponseParseError> {
    let path = format!("hookSpecificOutput.decision.{field}");
    validate_optional_field_type_at(object, field, &path, expected, is_expected)
}

fn validate_required_field_type_at<'a>(
    object: lingxi_core::types::utf16_json::JsonValueView<'a>,
    field: &str,
    path: &str,
    expected: &'static str,
    is_expected: fn(lingxi_core::types::utf16_json::JsonValueView<'a>) -> bool,
) -> Result<(), HookResponseParseError> {
    let Some(value) = object.get_property(field) else {
        return Err(HookResponseParseError::MissingSchemaField {
            field: path.to_string(),
            expected,
        });
    };
    if !is_expected(value) {
        return Err(HookResponseParseError::Schema {
            field: path.to_string(),
            expected,
            actual: json_value_type(value),
        });
    }
    Ok(())
}

fn validate_permission_update_array(
    value: lingxi_core::types::utf16_json::JsonValueView<'_>,
) -> Result<(), HookResponseParseError> {
    let Some(length) = value.array_len() else {
        return Err(HookResponseParseError::Schema {
            field: "hookSpecificOutput.decision.updatedPermissions".to_string(),
            expected: "array",
            actual: json_value_type(value),
        });
    };
    for index in 0..length {
        let update = value.get_index(index).expect("array index is in range");
        let path =
            |field: &str| format!("hookSpecificOutput.decision.updatedPermissions.{index}.{field}");
        if !is_json_object(update) {
            return Err(HookResponseParseError::Schema {
                field: format!("hookSpecificOutput.decision.updatedPermissions.{index}"),
                expected: "object",
                actual: json_value_type(update),
            });
        }
        validate_required_field_type_at(update, "type", &path("type"), "string", is_json_string)?;
        let kind = update
            .get_property("type")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        if !matches!(
            kind,
            "addRules"
                | "replaceRules"
                | "removeRules"
                | "setMode"
                | "addDirectories"
                | "removeDirectories"
        ) {
            return Err(HookResponseParseError::Schema {
                field: path("type"),
                expected: "\"addRules\" | \"replaceRules\" | \"removeRules\" | \"setMode\" | \"addDirectories\" | \"removeDirectories\"",
                actual: "invalid value",
            });
        }
        validate_required_field_type_at(
            update,
            "destination",
            &path("destination"),
            "string",
            is_json_string,
        )?;
        let destination = update
            .get_property("destination")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        if !matches!(
            destination,
            "userSettings" | "projectSettings" | "localSettings" | "session" | "cliArg"
        ) {
            return Err(HookResponseParseError::Schema {
                field: path("destination"),
                expected: "\"userSettings\" | \"projectSettings\" | \"localSettings\" | \"session\" | \"cliArg\"",
                actual: "invalid value",
            });
        }
        match kind {
            "addRules" | "replaceRules" | "removeRules" => {
                validate_required_field_type_at(
                    update,
                    "rules",
                    &path("rules"),
                    "array",
                    is_json_array,
                )?;
                let rules = update.get_property("rules").expect("rules was validated");
                let rules_len = rules.array_len().expect("rules was validated as an array");
                for rule_index in 0..rules_len {
                    let rule_path = |field: &str| {
                        format!(
                            "hookSpecificOutput.decision.updatedPermissions.{index}.rules.{rule_index}.{field}"
                        )
                    };
                    let rule = rules
                        .get_index(rule_index)
                        .expect("array index is in range");
                    if !is_json_object(rule) {
                        return Err(HookResponseParseError::Schema {
                            field: rule_path(""),
                            expected: "object",
                            actual: json_value_type(rule),
                        });
                    }
                    validate_required_field_type_at(
                        rule,
                        "toolName",
                        &rule_path("toolName"),
                        "string",
                        is_json_string,
                    )?;
                    validate_optional_field_type_at(
                        rule,
                        "ruleContent",
                        &rule_path("ruleContent"),
                        "string",
                        is_json_string,
                    )?;
                }
                validate_required_field_type_at(
                    update,
                    "behavior",
                    &path("behavior"),
                    "string",
                    is_json_string,
                )?;
                let behavior = update
                    .get_property("behavior")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                if !matches!(behavior, "allow" | "deny" | "ask") {
                    return Err(HookResponseParseError::Schema {
                        field: path("behavior"),
                        expected: "\"allow\" | \"deny\" | \"ask\"",
                        actual: "invalid value",
                    });
                }
            }
            "setMode" => {
                validate_required_field_type_at(
                    update,
                    "mode",
                    &path("mode"),
                    "string",
                    is_json_string,
                )?;
                let mode = update
                    .get_property("mode")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                if !matches!(
                    mode,
                    "default" | "acceptEdits" | "bypassPermissions" | "plan" | "dontAsk" | "auto"
                ) {
                    return Err(HookResponseParseError::Schema {
                        field: path("mode"),
                        expected: "\"default\" | \"acceptEdits\" | \"bypassPermissions\" | \"plan\" | \"dontAsk\" | \"auto\"",
                        actual: "invalid value",
                    });
                }
            }
            "addDirectories" | "removeDirectories" => {
                validate_required_field_type_at(
                    update,
                    "directories",
                    &path("directories"),
                    "array",
                    is_json_array,
                )?;
                let directories = update
                    .get_property("directories")
                    .expect("directories was validated");
                let directories_len = directories
                    .array_len()
                    .expect("directories was validated as an array");
                for directory_index in 0..directories_len {
                    let directory = directories
                        .get_index(directory_index)
                        .expect("array index is in range");
                    if !is_json_string(directory) {
                        return Err(HookResponseParseError::Schema {
                            field: format!(
                                "hookSpecificOutput.decision.updatedPermissions.{index}.directories.{directory_index}"
                            ),
                            expected: "string",
                            actual: json_value_type(directory),
                        });
                    }
                }
            }
            _ => unreachable!("validated permission update type"),
        }
    }
    Ok(())
}

fn validate_optional_string_array(
    object: lingxi_core::types::utf16_json::JsonValueView<'_>,
    field: &str,
) -> Result<(), HookResponseParseError> {
    let Some(value) = object.get_property(field) else {
        return Ok(());
    };
    let Some(length) = value.array_len() else {
        return Err(HookResponseParseError::Schema {
            field: format!("hookSpecificOutput.{field}"),
            expected: "array",
            actual: json_value_type(value),
        });
    };
    for index in 0..length {
        let item = value.get_index(index).expect("array index is in range");
        if !is_json_string(item) {
            return Err(HookResponseParseError::Schema {
                field: format!("hookSpecificOutput.{field}.{index}"),
                expected: "string",
                actual: json_value_type(item),
            });
        }
    }
    Ok(())
}

fn validate_permission_request_decision(
    object: lingxi_core::types::utf16_json::JsonValueView<'_>,
) -> Result<(), HookResponseParseError> {
    let Some(value) = object.get_property("decision") else {
        return Err(HookResponseParseError::MissingSchemaField {
            field: "hookSpecificOutput.decision".to_string(),
            expected: "object",
        });
    };
    if !is_json_object(value) {
        return Err(HookResponseParseError::Schema {
            field: "hookSpecificOutput.decision".to_string(),
            expected: "object",
            actual: json_value_type(value),
        });
    }
    validate_optional_permission_decision_field_type(value, "behavior", "string", is_json_string)?;
    let Some(behavior) = value
        .get_property("behavior")
        .and_then(|field| field.as_str())
    else {
        return Err(HookResponseParseError::MissingSchemaField {
            field: "hookSpecificOutput.decision.behavior".to_string(),
            expected: "string",
        });
    };
    match behavior {
        "allow" => {
            validate_optional_permission_decision_field_type(
                value,
                "updatedInput",
                "object",
                is_json_object,
            )?;
            validate_optional_permission_decision_field_type(
                value,
                "updatedPermissions",
                "array",
                is_json_array,
            )?;
            if let Some(updated_permissions) = value.get_property("updatedPermissions") {
                validate_permission_update_array(updated_permissions)?;
            }
        }
        "deny" => {
            validate_optional_permission_decision_field_type(
                value,
                "message",
                "string",
                is_json_string,
            )?;
            validate_optional_permission_decision_field_type(
                value,
                "interrupt",
                "boolean",
                is_json_boolean,
            )?;
        }
        _ => {
            return Err(HookResponseParseError::Schema {
                field: "hookSpecificOutput.decision.behavior".to_string(),
                expected: "\"allow\" | \"deny\"",
                actual: "invalid value",
            });
        }
    }
    Ok(())
}

/// PARITY 2.1.263 `imr(output, zodError)` — the three HINTS appended to a hook
/// JSON validation failure. They tell a hook author which field they reached for
/// instead of the one the event actually accepts, and they are keyed on the RAW
/// output shape (not on the validator's internal issue list), so they port
/// cleanly even though this crate validates with serde rather than zod.
///
/// ```js
/// if (isObj(hso) && !("hookEventName" in hso))
///   p = 'hookSpecificOutput is missing required field "hookEventName"';
/// else if (isObj(hso) && hso.hookEventName === "PermissionRequest" && !isObj(hso.decision) && …)
///   p += ' (PermissionRequest decision must be …)';
/// else if (isObj(e) && d?.path.length === 1 && d.path[0] === "decision" && …)
///   p += ' (top-level decision is the legacy approve|block field; …)';
/// ```
///
/// Returned separately from [`HookResponseParseError`] so callers can append it
/// to whichever diagnostic they surface; `None` when no hint applies.
#[must_use]
pub fn validation_hint(raw: &HookJson) -> Option<String> {
    let obj = raw.at_pointer("")?;
    if !is_json_object(obj) {
        return None;
    }
    if let Some(hso) = obj
        .get_property("hookSpecificOutput")
        .filter(|value| is_json_object(*value))
    {
        if hso.get_property("hookEventName").is_none() {
            // NB: the binary REPLACES the message here rather than appending.
            return Some(
                "hookSpecificOutput is missing required field \"hookEventName\"".to_string(),
            );
        }
        if hso
            .get_property("hookEventName")
            .and_then(|value| value.as_str())
            == Some("PermissionRequest")
            && !hso.get_property("decision").is_some_and(is_json_object)
        {
            return Some(
                " (PermissionRequest decision must be {\"behavior\": \"allow\"} or {\"behavior\": \"deny\", \"message\": \"...\"})"
                    .to_string(),
            );
        }
        return None;
    }
    // A hook that put `allow`/`deny`/`ask` in the LEGACY top-level `decision`.
    match obj
        .get_property("decision")
        .and_then(|value| value.as_str())
    {
        Some("ask") => Some(
            " (top-level decision is the legacy approve|block field; for \"ask\" use hookSpecificOutput.permissionDecision in a PreToolUse hook)"
                .to_string(),
        ),
        Some(other @ ("allow" | "deny")) => Some(format!(
            " (top-level decision is the legacy approve|block field; for \"{other}\" use hookSpecificOutput.permissionDecision in a PreToolUse hook, or hookSpecificOutput.decision: {{\"behavior\": \"{other}\"}} in a PermissionRequest hook)"
        )),
        _ => None,
    }
}

/// Parse a hook's JSON reply into a [`HookResponse`].
///
/// `expected_event` is a supported hook event name (including
/// `"PreModelSwitch"`/`"PostModelSwitch"`) — validates the
/// nested `hookSpecificOutput.hookEventName` per `hooks.ts:585` and rejects
/// known output fields whose JSON types do not match the hook schema.
pub fn parse_response(
    raw: &str,
    expected_event: &'static str,
) -> Result<HookResponse, HookResponseParseError> {
    let projection = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(raw)
        .map_err(|e| HookResponseParseError::Json(e.to_string()))?;
    parse_response_projection(&projection, expected_event)
}

/// Parse and validate a hook response already held in the core UTF-16 JSON
/// projection. Function hooks use this entry point so QuickJS strings retain
/// their exact units without a projection-to-JSON-to-projection round trip.
pub fn parse_response_projection(
    projection: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    expected_event: &'static str,
) -> Result<HookResponse, HookResponseParseError> {
    projection
        .validate()
        .map_err(|e| HookResponseParseError::Json(e.to_string()))?;
    let obj = projection
        .at_pointer("")
        .filter(|value| value.object_entries().is_some())
        .ok_or(HookResponseParseError::NotObject)?;

    // Claude Code validates the known output schema before interpreting any
    // decision. Keep unknown fields forward-compatible, but never silently
    // coerce a known field of the wrong JSON type into an absent field.
    validate_optional_field_type(obj, "continue", "boolean", is_json_boolean)?;
    validate_optional_field_type(obj, "stopReason", "string", is_json_string)?;
    validate_optional_field_type(obj, "suppressOutput", "boolean", is_json_boolean)?;
    validate_optional_field_type(obj, "systemMessage", "string", is_json_string)?;
    validate_optional_field_type(obj, "terminalSequence", "string", is_json_string)?;
    validate_optional_field_type(obj, "decision", "string", is_json_string)?;
    validate_optional_field_type(obj, "reason", "string", is_json_string)?;
    validate_optional_field_type(obj, "hookSpecificOutput", "object", is_json_object)?;
    let mut resp = HookResponse::default();

    // continue / stopReason. `continue: false` is the *preventContinuation*
    // signal for lifecycle (Stop / SubagentStop / TaskCompleted) hooks
    // (claude-code `hooks.ts:404`, `query.ts:1278`): it terminates the agent
    // loop regardless of any `decision: block`. For non-lifecycle events the
    // orchestrator ignores `prevent_continuation`, so this stays behavior-neutral.
    let cont = obj
        .get_property("continue")
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
    if !cont {
        resp.prevent_continuation = true;
        if obj.get_property("stopReason").is_some() {
            resp.reason = ExactHookText::from_json_projection(projection, "/stopReason");
        }
    }

    if let Some(suppress) = obj
        .get_property("suppressOutput")
        .and_then(|value| value.as_bool())
    {
        resp.suppress_output = suppress;
    }
    if obj.get_property("systemMessage").is_some() {
        resp.system_message = ExactHookText::from_json_projection(projection, "/systemMessage");
    }

    // Top-level `terminalSequence` is captured for all hook result types.
    if obj.get_property("terminalSequence").is_some() {
        resp.terminal_sequence =
            ExactHookText::from_json_projection(projection, "/terminalSequence");
    }

    // Legacy top-level decision uses a JS truthiness guard: empty string is
    // skipped, but an unrecognized non-empty string rejects the response.
    match obj
        .get_property("decision")
        .and_then(|value| value.as_str())
    {
        None | Some("") => {}
        Some("block") => resp.decision = Some(HookDecision::Block),
        Some("approve") => resp.decision = Some(HookDecision::Approve),
        Some(other) => {
            return Err(HookResponseParseError::UnknownDecision {
                value: other.to_string(),
            });
        }
    }

    // The binary never reads a bare top-level `permissionDecision`.
    if expected_event != "PermissionRequest" && obj.get_property("reason").is_some() {
        resp.reason = ExactHookText::from_json_projection(projection, "/reason");
    }

    if let Some(hs) = obj.get_property("hookSpecificOutput") {
        let name = hs.get_property("hookEventName").ok_or_else(|| {
            HookResponseParseError::MissingSchemaField {
                field: "hookSpecificOutput.hookEventName".to_string(),
                expected: "string",
            }
        })?;
        let name = name
            .as_str()
            .ok_or_else(|| HookResponseParseError::Schema {
                field: "hookSpecificOutput.hookEventName".to_string(),
                expected: "string",
                actual: json_value_type(name),
            })?;
        if name != expected_event {
            return Err(HookResponseParseError::EventNameMismatch {
                expected: expected_event,
                got: name.to_string(),
            });
        }

        if expected_event != "PermissionRequest" {
            validate_optional_nested_field_type(hs, "updatedInput", "object", is_json_object)?;
        }
        validate_optional_nested_field_type(hs, "classifierContext", "string", is_json_string)?;
        validate_optional_nested_field_type(hs, "additionalContext", "string", is_json_string)?;
        validate_optional_nested_field_type(hs, "retry", "boolean", is_json_boolean)?;
        validate_optional_nested_field_type(hs, "permissionDecision", "string", is_json_string)?;
        validate_optional_nested_field_type(
            hs,
            "permissionDecisionReason",
            "string",
            is_json_string,
        )?;
        validate_optional_nested_field_type(hs, "action", "string", is_json_string)?;
        validate_optional_nested_field_type(hs, "sessionTitle", "string", is_json_string)?;
        validate_optional_nested_field_type(
            hs,
            "suppressOriginalPrompt",
            "boolean",
            is_json_boolean,
        )?;
        validate_optional_nested_field_type(hs, "displayContent", "string", is_json_string)?;
        validate_optional_string_array(hs, "watchPaths")?;
        validate_optional_nested_field_type(hs, "initialUserMessage", "string", is_json_string)?;
        validate_optional_nested_field_type(hs, "reloadSkills", "boolean", is_json_boolean)?;

        if expected_event == "PermissionRequest" {
            validate_permission_request_decision(hs)?;
            let decision = hs
                .get_property("decision")
                .expect("validated PermissionRequest decision object");
            let behavior = decision
                .get_property("behavior")
                .and_then(|value| value.as_str())
                .expect("validated PermissionRequest behavior");
            match behavior {
                "allow" => {
                    let permission_decision = hs
                        .get_property("decision")
                        .expect("validated decision object");
                    let updated_input = if permission_decision
                        .get_property("updatedInput")
                        .is_some()
                    {
                        Some(
                            projection
                                .subprojection("/hookSpecificOutput/decision/updatedInput")
                                .map_err(|error| HookResponseParseError::Json(error.to_string()))?,
                        )
                    } else {
                        None
                    };
                    let updated_permissions = if permission_decision
                        .get_property("updatedPermissions")
                        .is_some()
                    {
                        Some(
                            projection
                                .subprojection("/hookSpecificOutput/decision/updatedPermissions")
                                .map_err(|error| HookResponseParseError::Json(error.to_string()))?,
                        )
                    } else {
                        None
                    };
                    resp.decision = Some(HookDecision::Allow);
                    resp.updated_input = updated_input.clone();
                    resp.updated_permissions = updated_permissions.clone();
                    resp.permission_request_result = Some(PermissionRequestResult::Allow {
                        updated_input,
                        updated_permissions,
                    });
                }
                "deny" => {
                    let decision_pointer = hs
                        .get_property("decision")
                        .expect("validated decision object");
                    let message = if decision_pointer.get_property("message").is_some() {
                        ExactHookText::from_json_projection(
                            projection,
                            "/hookSpecificOutput/decision/message",
                        )
                    } else {
                        None
                    };
                    let interrupt = decision_pointer
                        .get_property("interrupt")
                        .and_then(|value| value.as_bool());
                    resp.decision = Some(HookDecision::Block);
                    resp.reason = message.clone();
                    resp.interrupt = interrupt;
                    resp.permission_request_result =
                        Some(PermissionRequestResult::Deny { message, interrupt });
                }
                _ => unreachable!("validated PermissionRequest behavior"),
            }
        } else if hs.get_property("updatedInput").is_some() {
            resp.updated_input = Some(
                projection
                    .subprojection("/hookSpecificOutput/updatedInput")
                    .map_err(|error| HookResponseParseError::Json(error.to_string()))?,
            );
        }

        if expected_event == "PostToolUse" {
            if hs.get_property("updatedToolOutput").is_some() {
                resp.updated_tool_output = Some(
                    projection
                        .subprojection("/hookSpecificOutput/updatedToolOutput")
                        .map_err(|error| HookResponseParseError::Json(error.to_string()))?,
                );
            }
            if let Some(output) = hs.get_property("updatedMCPToolOutput") {
                if !output.is_null() {
                    resp.updated_mcp_tool_output = Some(
                        projection
                            .subprojection("/hookSpecificOutput/updatedMCPToolOutput")
                            .map_err(|error| HookResponseParseError::Json(error.to_string()))?,
                    );
                }
            }
            if let Some(context) = hs
                .get_property("classifierContext")
                .and_then(|value| value.as_str())
            {
                if !context.is_empty() {
                    resp.classifier_context = ExactHookText::from_json_projection(
                        projection,
                        "/hookSpecificOutput/classifierContext",
                    );
                }
            }
        }

        if let Some(context) = hs
            .get_property("additionalContext")
            .and_then(|value| value.as_str())
        {
            let _ = context;
            resp.additional_context = ExactHookText::from_json_projection(
                projection,
                "/hookSpecificOutput/additionalContext",
            );
        }
        if expected_event == "PermissionDenied" {
            if let Some(retry) = hs.get_property("retry").and_then(|value| value.as_bool()) {
                resp.retry = Some(retry);
            }
        }

        if matches!(expected_event, "PreToolUse" | "PreModelSwitch") {
            match hs
                .get_property("permissionDecision")
                .and_then(|value| value.as_str())
            {
                None | Some("") => {}
                Some("allow") => resp.decision = Some(HookDecision::Approve),
                Some("deny") => resp.decision = Some(HookDecision::Block),
                Some("ask") => resp.decision = Some(HookDecision::Ask),
                Some("defer") if expected_event == "PreToolUse" => {
                    resp.decision = Some(HookDecision::Defer)
                }
                Some(other) => {
                    return Err(HookResponseParseError::UnknownPermissionDecision {
                        value: other.to_string(),
                    });
                }
            }
        }
        if expected_event != "PermissionRequest"
            && hs.get_property("permissionDecisionReason").is_some()
        {
            resp.reason = ExactHookText::from_json_projection(
                projection,
                "/hookSpecificOutput/permissionDecisionReason",
            );
        }

        if let Some(action) = hs.get_property("action").and_then(|value| value.as_str()) {
            let content = if hs.get_property("content").is_some() {
                Some(
                    projection
                        .subprojection("/hookSpecificOutput/content")
                        .map_err(|error| HookResponseParseError::Json(error.to_string()))?,
                )
            } else {
                None
            };
            resp.elicitation_response = Some(crate::response::ElicitationHookResponse {
                action: action.to_owned(),
                content,
            });
            if action == "decline" {
                resp.decision = Some(HookDecision::Block);
            }
        }

        if expected_event == "UserPromptSubmit" {
            if hs.get_property("sessionTitle").is_some() {
                resp.session_title = ExactHookText::from_json_projection(
                    projection,
                    "/hookSpecificOutput/sessionTitle",
                );
            }
            if let Some(suppress) = hs
                .get_property("suppressOriginalPrompt")
                .and_then(|value| value.as_bool())
            {
                resp.suppress_original_prompt = suppress;
            }
        }
        if expected_event == "MessageDisplay" && hs.get_property("displayContent").is_some() {
            resp.display_content = ExactHookText::from_json_projection(
                projection,
                "/hookSpecificOutput/displayContent",
            );
        }
        if let Some(paths) = hs.get_property("watchPaths") {
            let length = paths
                .array_len()
                .expect("watchPaths was validated as an array");
            resp.watch_paths = Some(
                (0..length)
                    .map(|index| {
                        ExactHookText::from_json_projection(
                            projection,
                            &format!("/hookSpecificOutput/watchPaths/{index}"),
                        )
                        .expect("watchPaths items were validated as strings")
                    })
                    .collect(),
            );
        }
        if expected_event == "SessionStart" {
            if hs
                .get_property("initialUserMessage")
                .and_then(|value| value.as_str())
                .is_some()
            {
                resp.initial_user_message = ExactHookText::from_json_projection(
                    projection,
                    "/hookSpecificOutput/initialUserMessage",
                );
            }
            if let Some(reload) = hs
                .get_property("reloadSkills")
                .and_then(|value| value.as_bool())
            {
                resp.reload_skills = Some(reload);
            }
        }
    }

    Ok(resp)
}

#[cfg(test)]
#[path = "hook_payload_test.rs"]
mod hook_payload_test;

#[cfg(test)]
mod flat_response_projection_tests {
    use super::{parse_response_projection, HookJson};

    #[test]
    fn response_projection_keeps_arbitrary_text_units_and_null_presence() {
        let source = r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedToolOutput":{"text":"\ud800","extra":[null]}}}"#;
        let projection = HookJson::parse(source).unwrap();
        let response = parse_response_projection(&projection, "PostToolUse").unwrap();
        let output = response.updated_tool_output.unwrap();
        assert_eq!(
            output.to_json_string().unwrap(),
            r#"{"text":"\ud800","extra":[null]}"#
        );

        let absent =
            HookJson::parse(r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse"}}"#).unwrap();
        assert!(parse_response_projection(&absent, "PostToolUse")
            .unwrap()
            .updated_tool_output
            .is_none());

        let explicit_null = HookJson::parse(
            r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedToolOutput":null}}"#,
        )
        .unwrap();
        assert!(parse_response_projection(&explicit_null, "PostToolUse")
            .unwrap()
            .updated_tool_output
            .unwrap()
            .at_pointer("")
            .is_some_and(|value| value.is_null()));
    }

    #[test]
    fn response_projection_does_not_materialize_deep_arbitrary_updates() {
        let deep = format!("{}null{}", "[".repeat(512), "]".repeat(512));
        let source = format!(
            r#"{{"hookSpecificOutput":{{"hookEventName":"PostToolUse","updatedToolOutput":{deep}}}}}"#
        );
        let projection = HookJson::parse(&source).unwrap();
        let response = parse_response_projection(&projection, "PostToolUse").unwrap();
        let output = response.updated_tool_output.unwrap();
        assert_eq!(output.to_json_string().unwrap(), deep);
    }

    #[test]
    fn classifier_context_captures_exact_units_from_source() {
        let projection = HookJson::parse(
            r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","classifierContext":"\ud800"}}"#,
        )
        .unwrap();
        let response = parse_response_projection(&projection, "PostToolUse").unwrap();
        assert_eq!(
            response.classifier_context.unwrap().utf16_code_units,
            [0xD800]
        );
    }
}
