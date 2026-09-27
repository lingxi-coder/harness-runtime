//! F1-05 — Listing / screen DTO round-trip tests.
//!
//! Freezes the pull/reply DTOs for every client screen (sessions, models, MCP,
//! hooks, agents, slash commands, memory, status, settings, auth, doctor,
//! tasks). Each DTO gets a serialize → assert-tag/field → deserialize →
//! assert-eq round-trip so the wire shape is locked before the F1-08 snapshot
//! golden is generated (plan F1-05).
//!
//! Name reconciliation (plan line 149): the design spec §4.1 says `AgentList`,
//! but the WIRE name is `Agents` — reconciled here while the snapshot is still
//! unfrozen.
//!
//! `serde_json` is a DEV-ONLY dep — the contract crate itself never depends on
//! `serde_json::Value` (governing decision §0.4): `effective_json` /
//! `provenance_json` are JSON **Strings**, not nested objects.

use client_protocol::controls::{ReasoningControlSpecDto, ReasoningSelectionDto};
use client_protocol::events::ClientEvent;
use client_protocol::listings::{
    AgentDto, AuthStateDto, CheckStatusDto, CoordinatorWorkerDto, DoctorCheckDto, DoctorReportDto,
    DoctorSummaryDto, HookDto, McpServerDto, McpStatusDto, MemoryEntryDto, MemoryTierDto,
    ModelBillingModeDto, ModelCapabilitiesDto, ModelDetailsDto, ModelPricingDto,
    ProviderModelCatalogEntryDto, SessionModeDto, SessionRowDto, SlashCommandDto,
    StatusSnapshotDto, TaskRowDto, TaskStatusDto,
};
use client_protocol::message::{MessageBlockDto, MessageDto};

// ── Sessions ─────────────────────────────────────────────────────────────────

/// `SessionList` — the resumable-session catalog. Maps `SessionMetadata`; the
/// row carries `.path` DIRECTLY (it exists at `session/src/jsonl/loader.rs:33`,
/// plan line 152 — do NOT synthesize).
#[test]
fn session_list_round_trips() {
    let ev = ClientEvent::SessionList {
        sessions: vec![SessionRowDto {
            uuid: "0b3e2f10-1234-4abc-8def-0123456789ab".to_string(),
            title: "Refactor the parser".to_string(),
            modified_rfc3339: "2026-06-02T15:04:05Z".to_string(),
            message_count: 42,
            mode: SessionModeDto::Code,
            path: "/Users/x/.lingxi/projects/p/0b3e2f10.jsonl".to_string(),
        }],
    };
    let json = serde_json::to_value(&ev).expect("serialize SessionList");
    assert_eq!(json["type"], "session_list");
    assert_eq!(json["sessions"][0]["message_count"], 42);
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize SessionList");
    assert_eq!(back, ev);
}

/// The plan's named structural anchor (plan line 156): the session row carries
/// `path` directly — a present, non-empty field on the wire.
#[test]
fn session_row_carries_path() {
    let row = SessionRowDto {
        uuid: "u".to_string(),
        title: "t".to_string(),
        modified_rfc3339: "2026-06-02T15:04:05Z".to_string(),
        message_count: 1,
        mode: SessionModeDto::Chat,
        path: "/abs/path/to/s.jsonl".to_string(),
    };
    let json = serde_json::to_value(&row).expect("serialize SessionRowDto");
    assert!(
        json.get("path").is_some(),
        "SessionRowDto must carry `path`"
    );
    assert_eq!(json["path"], "/abs/path/to/s.jsonl");
    let back: SessionRowDto = serde_json::from_value(json).expect("deserialize SessionRowDto");
    assert_eq!(back, row);
}

/// `SessionStarted`/`SessionResumed` carry the `session_id` as a CONNECTION
/// ATTRIBUTE (decision §0.5) — it travels on these lifecycle events, never as a
/// per-command param. `SessionEnded` is a unit-style marker.
#[test]
fn session_lifecycle_events_round_trip() {
    let started = ClientEvent::SessionStarted {
        session_id: "sess-1".to_string(),
        mode: SessionModeDto::Code,
    };
    let json = serde_json::to_value(&started).expect("serialize SessionStarted");
    assert_eq!(json["type"], "session_started");
    assert_eq!(json["session_id"], "sess-1");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize SessionStarted");
    assert_eq!(back, started);

    // `SessionResumed` now also carries the full restored transcript as
    // `messages` (OLDEST-FIRST, REQUIRED — always present, may be empty) so the
    // client renders the rehydrated conversation atomically on resume.
    let resumed = ClientEvent::SessionResumed {
        session_id: "sess-2".to_string(),
        mode: SessionModeDto::Chat,
        messages: vec![MessageDto {
            loop_wakeup: None,
            role: "user".to_string(),
            blocks: vec![MessageBlockDto::Text {
                text: "prior turn".to_string(),
            }],
            images: Vec::new(),
        }],
    };
    let json = serde_json::to_value(&resumed).expect("serialize SessionResumed");
    assert_eq!(json["type"], "session_resumed");
    assert_eq!(json["session_id"], "sess-2");
    // `messages` is always present on the wire (REQUIRED, oldest-first).
    assert_eq!(json["messages"][0]["role"], "user");
    assert_eq!(json["messages"][0]["blocks"][0]["type"], "text");
    assert_eq!(json["messages"][0]["blocks"][0]["text"], "prior turn");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize SessionResumed");
    assert_eq!(back, resumed);

    let ended = ClientEvent::SessionEnded;
    let json = serde_json::to_value(&ended).expect("serialize SessionEnded");
    assert_eq!(json["type"], "session_ended");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize SessionEnded");
    assert_eq!(back, ended);
}

// ── Models ───────────────────────────────────────────────────────────────────

/// `ModelList { models, current }` and `ModelChanged { model }`.
#[test]
fn model_list_and_changed_round_trip() {
    let list = ClientEvent::ModelList {
        models: vec![
            "claude-opus-4-7".to_string(),
            "claude-sonnet-4-6".to_string(),
        ],
        current: "claude-opus-4-7".to_string(),
        details: Vec::new(),
    };
    let json = serde_json::to_value(&list).expect("serialize ModelList");
    assert_eq!(json["type"], "model_list");
    assert_eq!(json["current"], "claude-opus-4-7");
    assert_eq!(json["models"][1], "claude-sonnet-4-6");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize ModelList");
    assert_eq!(back, list);

    let changed = ClientEvent::ModelChanged {
        model: "claude-sonnet-4-6".to_string(),
    };
    let json = serde_json::to_value(&changed).expect("serialize ModelChanged");
    assert_eq!(json["type"], "model_changed");
    assert_eq!(json["model"], "claude-sonnet-4-6");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize ModelChanged");
    assert_eq!(back, changed);
}

#[test]
fn rich_model_details_round_trip_and_legacy_event_default() {
    let detail = ModelDetailsDto {
        reference: "deepseek/deepseek-flash".into(),
        provider_id: "deepseek".into(),
        provider_label: "DeepSeek".into(),
        display_name: "DeepSeek V4.1 Flash".into(),
        model_id: "deepseek-flash".into(),
        description: None,
        family: Some("deepseek-flash".into()),
        status: Some("beta".into()),
        release_date: None,
        last_updated: None,
        knowledge_cutoff: None,
        input_modalities: vec!["text".into(), "image".into()],
        output_modalities: vec!["text".into()],
        context_window_tokens: Some(1_000_000),
        max_input_tokens: None,
        max_output_tokens: Some(384_000),
        open_weights: None,
        attachments: None,
        temperature_control: None,
        pricing: Some(ModelPricingDto {
            billing_mode: ModelBillingModeDto::PerToken,
            input_per_million: Some(0.3),
            output_per_million: Some(1.2),
            cache_read_per_million: Some(0.006),
            cache_write_per_million: None,
            reasoning_per_million: Some(1.2),
            tiers: Vec::new(),
            source: Some("modelsDev".into()),
        }),
        capabilities: ModelCapabilitiesDto {
            streaming: true,
            tools: true,
            vision: true,
            documents: false,
            reasoning: true,
            structured_output: false,
        },
        reasoning: ReasoningControlSpecDto {
            options: Vec::new(),
            budget_range: None,
            provider_default: ReasoningSelectionDto::Automatic,
            forced_reasoning: false,
            editable: true,
            disabled_reason: None,
        },
        supports_fast_mode: true,
        fusion_analyst_capable: true,
    };
    let event = ClientEvent::ModelList {
        models: vec![detail.reference.clone()],
        current: detail.reference.clone(),
        details: vec![detail],
    };
    let value = serde_json::to_value(&event).unwrap();
    assert_eq!(value["details"][0]["status"], "beta");
    assert_eq!(serde_json::from_value::<ClientEvent>(value).unwrap(), event);

    let legacy = serde_json::json!({
        "type": "model_list",
        "models": ["openai/gpt-5.6-sol"],
        "current": "openai/gpt-5.6-sol"
    });
    let ClientEvent::ModelList { details, .. } =
        serde_json::from_value::<ClientEvent>(legacy).unwrap()
    else {
        panic!("model list")
    };
    assert!(details.is_empty());
}

#[test]
fn provider_model_catalog_round_trips() {
    let detail = ModelDetailsDto {
        reference: "anthropic/claude-opus-4-8".into(),
        provider_id: "anthropic".into(),
        provider_label: "Anthropic".into(),
        display_name: "Claude Opus 4.8".into(),
        model_id: "claude-opus-4-8".into(),
        description: Some("Large reasoning model".into()),
        family: Some("claude".into()),
        status: None,
        release_date: None,
        last_updated: None,
        knowledge_cutoff: None,
        input_modalities: vec!["text".into()],
        output_modalities: vec!["text".into()],
        context_window_tokens: None,
        max_input_tokens: None,
        max_output_tokens: None,
        open_weights: None,
        attachments: None,
        temperature_control: None,
        pricing: None,
        capabilities: ModelCapabilitiesDto {
            streaming: true,
            tools: true,
            vision: false,
            documents: false,
            reasoning: true,
            structured_output: false,
        },
        reasoning: ReasoningControlSpecDto {
            options: Vec::new(),
            budget_range: None,
            provider_default: ReasoningSelectionDto::Automatic,
            forced_reasoning: false,
            editable: true,
            disabled_reason: None,
        },
        supports_fast_mode: false,
        fusion_analyst_capable: false,
    };
    let event = ClientEvent::ProviderModelCatalog {
        providers: vec![ProviderModelCatalogEntryDto {
            provider_id: "anthropic".into(),
            provider_label: "Anthropic".into(),
            models: vec![detail],
            group: None,
            connection_id: None,
        }],
    };
    let value = serde_json::to_value(&event).unwrap();
    assert_eq!(value["type"], "provider_model_catalog");
    assert_eq!(value["providers"][0]["provider_label"], "Anthropic");
    assert_eq!(serde_json::from_value::<ClientEvent>(value).unwrap(), event);
}

// ── MCP ──────────────────────────────────────────────────────────────────────

/// `McpServers { servers }` round-trips; the row mirrors `McpServerInfo`
/// (`platform-api/src/orchestrator.rs:122`).
#[test]
fn mcp_servers_round_trips() {
    let ev = ClientEvent::McpServers {
        servers: vec![
            McpServerDto {
                name: "github".to_string(),
                status: McpStatusDto::Connected,
                transport: "stdio".to_string(),
            },
            McpServerDto {
                name: "sentry".to_string(),
                status: McpStatusDto::Disconnected,
                transport: "sse".to_string(),
            },
        ],
    };
    let json = serde_json::to_value(&ev).expect("serialize McpServers");
    assert_eq!(json["type"], "mcp_servers");
    assert_eq!(json["servers"][0]["name"], "github");
    assert_eq!(json["servers"][0]["status"]["type"], "connected");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize McpServers");
    assert_eq!(back, ev);
}

/// `McpStatus::Error(String)` (a tuple variant in the engine) is lowered to a
/// STRUCT variant `McpStatusDto::Error { reason }` for `UniFFI` flatness (plan
/// line 154 + the named test `mcp_status_error_is_struct_variant`).
#[test]
fn mcp_status_error_is_struct_variant() {
    let status = McpStatusDto::Error {
        reason: "connection refused".to_string(),
    };
    let json = serde_json::to_value(&status).expect("serialize McpStatusDto::Error");
    assert_eq!(json["type"], "error");
    // It is a STRUCT variant — `reason` is a named field, not a positional
    // tuple element (which serde would serialize differently).
    assert_eq!(json["reason"], "connection refused");
    let back: McpStatusDto = serde_json::from_value(json).expect("deserialize McpStatusDto::Error");
    assert_eq!(back, status);

    // All three status variants round-trip with snake_case tags.
    for (s, tag) in [
        (McpStatusDto::Connected, "connected"),
        (McpStatusDto::Disconnected, "disconnected"),
    ] {
        let json = serde_json::to_value(&s).expect("serialize McpStatusDto");
        assert_eq!(json["type"], tag);
        let back: McpStatusDto = serde_json::from_value(json).expect("deserialize McpStatusDto");
        assert_eq!(back, s);
    }
}

// ── Hooks ────────────────────────────────────────────────────────────────────

/// `Hooks { hooks }` round-trips; the row mirrors `HookInfo`
/// (`platform-api/src/orchestrator.rs:144`). `matcher` is optional and skipped when
/// absent.
#[test]
fn hooks_round_trips() {
    let ev = ClientEvent::Hooks {
        hooks: vec![
            HookDto {
                name: "format-on-save".to_string(),
                event: "PostToolUse".to_string(),
                matcher: Some("Edit|Write".to_string()),
                timeout_ms: 60_000,
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
            HookDto {
                name: "notify".to_string(),
                event: "Stop".to_string(),
                matcher: None,
                timeout_ms: 5_000,
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
        ],
    };
    let json = serde_json::to_value(&ev).expect("serialize Hooks");
    assert_eq!(json["type"], "hooks");
    assert_eq!(json["hooks"][0]["matcher"], "Edit|Write");
    // None matcher is skipped on the wire.
    assert!(
        json["hooks"][1].get("matcher").is_none(),
        "None matcher must be skipped"
    );
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize Hooks");
    assert_eq!(back, ev);
}

// ── Agents (reconciled from spec's `AgentList`) ──────────────────────────────

/// `Agents { agents }` — the WIRE name (reconciled from spec §4.1 `AgentList`,
/// plan line 149). The row mirrors `AgentInfo` (`platform-api/src/orchestrator.rs:157`).
#[test]
fn agents_round_trips() {
    let ev = ClientEvent::Agents {
        agents: vec![AgentDto {
            name: "researcher".to_string(),
            description: "Deep web research".to_string(),
            tools_allowed: vec!["WebSearch".to_string(), "WebFetch".to_string()],
        }],
    };
    let json = serde_json::to_value(&ev).expect("serialize Agents");
    assert_eq!(json["type"], "agents");
    assert_eq!(json["agents"][0]["name"], "researcher");
    assert_eq!(json["agents"][0]["tools_allowed"][0], "WebSearch");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize Agents");
    assert_eq!(back, ev);
}

// ── Slash commands ───────────────────────────────────────────────────────────

/// `SlashCommandCatalog { commands }` — mirrors the `SlashCommand` registry
/// shape collapsed to display fields (`command-api/src/model.rs:12`).
#[test]
fn slash_command_catalog_round_trips() {
    let ev = ClientEvent::SlashCommandCatalog {
        commands: vec![
            SlashCommandDto {
                name: "model".to_string(),
                description: "Switch the active model".to_string(),
                source: "builtin".to_string(),
                aliases: vec!["m".to_string()],
                argument_hint: Some("[model]".to_string()),
                menu_description: Some("Switch model".to_string()),
                hidden: false,
            },
            SlashCommandDto {
                name: "review".to_string(),
                description: "Review a PR".to_string(),
                source: "markdown".to_string(),
                aliases: vec![],
                argument_hint: None,
                menu_description: None,
                hidden: true,
            },
        ],
    };
    let json = serde_json::to_value(&ev).expect("serialize SlashCommandCatalog");
    assert_eq!(json["type"], "slash_command_catalog");
    assert_eq!(json["commands"][0]["name"], "model");
    assert_eq!(json["commands"][0]["aliases"][0], "m");
    assert_eq!(json["commands"][0]["argument_hint"], "[model]");
    assert_eq!(json["commands"][0]["menu_description"], "Switch model");
    assert_eq!(json["commands"][1]["source"], "markdown");
    assert_eq!(json["commands"][1]["hidden"], true);
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize SlashCommandCatalog");
    assert_eq!(back, ev);
}

// ── Memory ───────────────────────────────────────────────────────────────────

/// `MemoryEntries { entries }` — mirrors `protocol::MemoryEntry`
/// (`protocol/src/messages.rs:201`), tier lowered to a `snake_case` enum.
#[test]
fn memory_entries_round_trips() {
    let ev = ClientEvent::MemoryEntries {
        entries: vec![
            MemoryEntryDto {
                path: "/repo/LINGXI.md".to_string(),
                tier: MemoryTierDto::Project,
                body: "# Project rules".to_string(),
                age_days: 3,
                size_bytes: 128,
            },
            MemoryEntryDto {
                path: "/Users/x/.lingxi/LINGXI.md".to_string(),
                tier: MemoryTierDto::User,
                body: "# User rules".to_string(),
                age_days: 0,
                size_bytes: 64,
            },
        ],
    };
    let json = serde_json::to_value(&ev).expect("serialize MemoryEntries");
    assert_eq!(json["type"], "memory_entries");
    assert_eq!(json["entries"][0]["tier"]["type"], "project");
    assert_eq!(json["entries"][1]["tier"]["type"], "user");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize MemoryEntries");
    assert_eq!(back, ev);
}

/// All four memory tiers round-trip with `snake_case` tags (mirrors
/// `protocol::MemoryEntryTier`).
#[test]
fn memory_tier_variants_round_trip() {
    for (tier, tag) in [
        (MemoryTierDto::Session, "session"),
        (MemoryTierDto::Project, "project"),
        (MemoryTierDto::Team, "team"),
        (MemoryTierDto::User, "user"),
    ] {
        // `MemoryTierDto` is `Copy`, so pass by value (no needless borrow).
        let json = serde_json::to_value(tier).expect("serialize MemoryTierDto");
        assert_eq!(json["type"], tag, "MemoryTierDto::{tier:?} tag mismatch");
        let back: MemoryTierDto = serde_json::from_value(json).expect("deserialize MemoryTierDto");
        assert_eq!(back, tier);
    }
}

// ── Status ───────────────────────────────────────────────────────────────────

/// `StatusSnapshot` — the `/status` panel. The traits-shape fields are
/// canonical; the status-line fields are appended OPTIONAL (plan line 155),
/// skipped when `None`.
#[test]
fn status_snapshot_round_trips() {
    let ev = ClientEvent::StatusSnapshot {
        snapshot: StatusSnapshotDto {
            session_id: "sess-1".to_string(),
            model: "claude-opus-4-7".to_string(),
            n_messages: 12,
            total_cost_usd: 0.0345,
            input_tokens: 4096,
            output_tokens: 512,
            n_mcp_connected: 2,
            n_mcp_total: 3,
            n_hooks: 4,
            n_agents: 1,
            started_at: "2026-06-02T15:00:00Z".to_string(),
            cwd: "/repo".to_string(),
            status_line: None,
            active_workers: None,
        },
    };
    let json = serde_json::to_value(&ev).expect("serialize StatusSnapshot");
    assert_eq!(json["type"], "status_snapshot");
    assert_eq!(json["snapshot"]["model"], "claude-opus-4-7");
    assert_eq!(json["snapshot"]["n_mcp_connected"], 2);
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize StatusSnapshot");
    assert_eq!(back, ev);
}

/// The named test `status_snapshot_optional_fields_skip_when_none` — the
/// appended status-line field is OPTIONAL and skipped from the wire when
/// `None`, but populated when `Some` (plan line 156).
#[test]
fn status_snapshot_optional_fields_skip_when_none() {
    let mut snap = StatusSnapshotDto {
        session_id: "s".to_string(),
        model: "m".to_string(),
        n_messages: 0,
        total_cost_usd: 0.0,
        input_tokens: 0,
        output_tokens: 0,
        n_mcp_connected: 0,
        n_mcp_total: 0,
        n_hooks: 0,
        n_agents: 0,
        started_at: "2026-06-02T15:00:00Z".to_string(),
        cwd: "/".to_string(),
        status_line: None,
        active_workers: None,
    };
    let json = serde_json::to_value(&snap).expect("serialize StatusSnapshotDto none");
    assert!(
        json.get("status_line").is_none(),
        "None status_line must be skipped on the wire"
    );
    assert!(
        json.get("active_workers").is_none(),
        "None active_workers must be skipped on the wire"
    );

    snap.status_line = Some("main ✓ | 3 changes".to_string());
    let json = serde_json::to_value(&snap).expect("serialize StatusSnapshotDto some");
    assert_eq!(json["status_line"], "main ✓ | 3 changes");
    let back: StatusSnapshotDto =
        serde_json::from_value(json).expect("deserialize StatusSnapshotDto some");
    assert_eq!(back, snap);
}

/// T21 — the appended optional `active_workers` scalar round-trips: skipped from
/// the wire when `None`, present and equal when `Some(n)`. This is the field
/// `/status` surfaces so it echoes the same count the PUSH `CoordinatorStatus`
/// feed carries.
#[test]
fn status_snapshot_carries_active_workers_roundtrip() {
    let mut snap = StatusSnapshotDto {
        session_id: "s".to_string(),
        model: "m".to_string(),
        n_messages: 0,
        total_cost_usd: 0.0,
        input_tokens: 0,
        output_tokens: 0,
        n_mcp_connected: 0,
        n_mcp_total: 0,
        n_hooks: 0,
        n_agents: 0,
        started_at: "2026-06-02T15:00:00Z".to_string(),
        cwd: "/".to_string(),
        status_line: None,
        active_workers: None,
    };
    // None → skipped from the wire (additive-optional convention).
    let json = serde_json::to_value(&snap).expect("serialize none active_workers");
    assert!(
        json.get("active_workers").is_none(),
        "None active_workers must be skipped on the wire"
    );

    // Some(n) → present, integer-typed, and round-trips byte-for-byte.
    snap.active_workers = Some(3);
    let json = serde_json::to_value(&snap).expect("serialize some active_workers");
    assert_eq!(json["active_workers"], 3);
    let back: StatusSnapshotDto =
        serde_json::from_value(json).expect("deserialize some active_workers");
    assert_eq!(back, snap);
    assert_eq!(back.active_workers, Some(3));
}

// ── Settings ─────────────────────────────────────────────────────────────────

/// `SettingsSnapshot { effective_json, provenance_json }` — both are JSON
/// **Strings** on the wire (decision §0.4), NOT nested objects.
#[test]
fn settings_snapshot_round_trips() {
    let ev = ClientEvent::SettingsSnapshot {
        effective_json: r#"{"model":"claude-opus-4-7","theme":"dark"}"#.to_string(),
        provenance_json: r#"{"model":"project","theme":"user"}"#.to_string(),
        files_json: None,
        active_json: None,
        locked: None,
        layers_json: None,
        merged_keys: None,
    };
    let json = serde_json::to_value(&ev).expect("serialize SettingsSnapshot");
    assert_eq!(json["type"], "settings_snapshot");
    // Both payloads are JSON Strings, not nested objects.
    assert!(
        json["effective_json"].is_string(),
        "effective_json must be a String"
    );
    assert!(
        json["provenance_json"].is_string(),
        "provenance_json must be a String"
    );
    // The five ADDED optional fields stay OFF the wire when unset, which is
    // what makes the addition additive: a client that predates them decodes
    // the identical two-field payload.
    let json = serde_json::to_value(&ev).expect("serialize SettingsSnapshot");
    assert!(json.get("files_json").is_none());
    assert!(json.get("active_json").is_none());
    assert!(json.get("locked").is_none());
    assert!(json.get("layers_json").is_none());
    assert!(json.get("merged_keys").is_none());
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize SettingsSnapshot");
    assert_eq!(back, ev);
}

/// The added fields carry the per-file layer states, the session's actually
/// loaded values, and the administrator-locked keys. `files_json`/`active_json`
/// are JSON **Strings** for the same §0.4 reason as the two required payloads;
/// `locked` is a plain string list, which IS UniFFI-representable.
#[test]
fn settings_snapshot_optional_fields_round_trip() {
    let ev = ClientEvent::SettingsSnapshot {
        effective_json: r#"{"model":"claude-opus-4-7"}"#.to_string(),
        provenance_json: r#"{"model":"project"}"#.to_string(),
        files_json: Some(
            r#"[{"layer":"project","path":"/repo/.lingxi/settings.json","exists":true,"writable":true}]"#
                .to_string(),
        ),
        active_json: Some(r#"{"model":"claude-opus-4-7"}"#.to_string()),
        locked: Some(vec!["telemetryEnabled".to_string()]),
        layers_json: Some(r#"{"project":{"model":"claude-opus-4-7"}}"#.to_string()),
        merged_keys: Some(vec!["hooks".to_string()]),
    };
    let json = serde_json::to_value(&ev).expect("serialize SettingsSnapshot");
    assert!(
        json["files_json"].is_string() && json["active_json"].is_string(),
        "the added structured payloads must be JSON Strings, not nested objects"
    );
    assert!(json["locked"].is_array());
    // `merged_keys` is a plain string list too — the keys whose effective
    // value is a cross-layer union, so a client knows not to draw a
    // single-layer provenance badge for them.
    assert!(json["merged_keys"].is_array());
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize SettingsSnapshot");
    assert_eq!(back, ev);
}

/// A payload written by a client that predates the three added fields must
/// still decode — that is what "additive" means for this variant.
#[test]
fn settings_snapshot_decodes_a_payload_without_the_added_fields() {
    let json = serde_json::json!({
        "type": "settings_snapshot",
        "effective_json": "{}",
        "provenance_json": "{}",
    });
    let back: ClientEvent = serde_json::from_value(json).expect("legacy payload must decode");
    assert_eq!(
        back,
        ClientEvent::SettingsSnapshot {
            effective_json: "{}".to_string(),
            provenance_json: "{}".to_string(),
            files_json: None,
            active_json: None,
            locked: None,
            layers_json: None,
            merged_keys: None,
        }
    );
}

// ── Auth ─────────────────────────────────────────────────────────────────────

/// `AuthState` — mirrors `Option<LoginInfo>` (`platform-api/src/auth.rs:13`) as a
/// tagged enum: signed-out carries no payload; signed-in carries email + org.
#[test]
fn auth_state_round_trips() {
    let signed_out = ClientEvent::AuthState {
        state: AuthStateDto::SignedOut,
    };
    let json = serde_json::to_value(&signed_out).expect("serialize AuthState SignedOut");
    assert_eq!(json["type"], "auth_state");
    assert_eq!(json["state"]["type"], "signed_out");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize AuthState SignedOut");
    assert_eq!(back, signed_out);

    let signed_in = ClientEvent::AuthState {
        state: AuthStateDto::SignedIn {
            email: "u@x.com".to_string(),
            org_id: "org_123".to_string(),
        },
    };
    let json = serde_json::to_value(&signed_in).expect("serialize AuthState SignedIn");
    assert_eq!(json["state"]["type"], "signed_in");
    assert_eq!(json["state"]["email"], "u@x.com");
    assert_eq!(json["state"]["org_id"], "org_123");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize AuthState SignedIn");
    assert_eq!(back, signed_in);
}

// ── Doctor ───────────────────────────────────────────────────────────────────

/// `DoctorReport { checks, summary }` — mirrors `DoctorReport`
/// (`platform-api/src/orchestrator.rs:168`). Each check carries an optional `detail`.
#[test]
fn doctor_report_round_trips() {
    let ev = ClientEvent::DoctorReport {
        report: DoctorReportDto {
            checks: vec![
                DoctorCheckDto {
                    name: "config-dir".to_string(),
                    status: CheckStatusDto::Pass,
                    detail: None,
                },
                DoctorCheckDto {
                    name: "api-key".to_string(),
                    status: CheckStatusDto::Warn,
                    detail: Some("using fallback key".to_string()),
                },
            ],
            summary: DoctorSummaryDto {
                passed: 1,
                warnings: 1,
                failed: 0,
            },
        },
    };
    let json = serde_json::to_value(&ev).expect("serialize DoctorReport");
    assert_eq!(json["type"], "doctor_report");
    assert_eq!(json["report"]["checks"][0]["status"]["type"], "pass");
    assert_eq!(json["report"]["checks"][1]["status"]["type"], "warn");
    // None detail is skipped.
    assert!(
        json["report"]["checks"][0].get("detail").is_none(),
        "None detail must be skipped"
    );
    assert_eq!(json["report"]["checks"][1]["detail"], "using fallback key");
    assert_eq!(json["report"]["summary"]["passed"], 1);
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize DoctorReport");
    assert_eq!(back, ev);
}

/// All three doctor check-status outcomes round-trip with `snake_case` tags.
#[test]
fn check_status_variants_round_trip() {
    for (status, tag) in [
        (CheckStatusDto::Pass, "pass"),
        (CheckStatusDto::Warn, "warn"),
        (CheckStatusDto::Fail, "fail"),
    ] {
        // `CheckStatusDto` is `Copy`, so pass by value (no needless borrow).
        let json = serde_json::to_value(status).expect("serialize CheckStatusDto");
        assert_eq!(json["type"], tag, "CheckStatusDto::{status:?} tag mismatch");
        let back: CheckStatusDto =
            serde_json::from_value(json).expect("deserialize CheckStatusDto");
        assert_eq!(back, status);
    }
}

// ── Tasks ────────────────────────────────────────────────────────────────────

/// `TaskRow` — one task row. Mirrors `TaskRecord`
/// (`platform-api/src/task_registry.rs:36`); the status wire string is lowered to a
/// `TaskStatusDto` enum.
#[test]
fn task_row_old_payload_defaults_new_display_fields_and_nondefaults_round_trip() {
    let old = serde_json::json!({"task_id":"a12345678", "task_type":"local_agent", "status":{"type":"completed"}, "description":"review"});
    let mut row: TaskRowDto = serde_json::from_value(old).unwrap();
    assert!(!row.unread);
    assert_eq!(row.model, None);
    assert_eq!(row.effort, None);
    assert_eq!(row.agent_id, None);
    let encoded = serde_json::to_value(&row).unwrap();
    for field in ["unread", "model", "effort", "agent_id"] {
        assert!(
            encoded.get(field).is_none(),
            "default {field} must remain wire-additive"
        );
    }
    row.agent_id = Some("agent:runner".into());
    row.unread = true;
    row.model = Some("resolved-model".into());
    row.effort = Some("high".into());
    let encoded = serde_json::to_value(&row).unwrap();
    assert_eq!(encoded["agent_id"], "agent:runner");
    assert_eq!(encoded["unread"], true);
    assert_eq!(encoded["model"], "resolved-model");
    assert_eq!(encoded["effort"], "high");
    assert_eq!(serde_json::from_value::<TaskRowDto>(encoded).unwrap(), row);
}

#[test]
fn task_row_round_trips() {
    let ev = ClientEvent::TaskRow {
        task: TaskRowDto {
            agent_id: None,
            unread: false,
            model: None,
            effort: None,
            kind: None,
            awaiting_plan_approval: false,
            task_id: "b1234abcd".to_string(),
            task_type: "background".to_string(),
            status: TaskStatusDto::Running,
            description: "Build the docs".to_string(),
            can_resume: false,
            started_at_ms: None,
            error: None,
            stage: None,
        },
    };
    let json = serde_json::to_value(&ev).expect("serialize TaskRow");
    assert_eq!(json["type"], "task_row");
    assert_eq!(json["task"]["task_id"], "b1234abcd");
    assert_eq!(json["task"]["status"]["type"], "running");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize TaskRow");
    assert_eq!(back, ev);
}

/// `TaskOutputChunk` — a chunk of a task's spool. Mirrors `TaskOutputChunk`
/// (`platform-api/src/task_registry.rs:49`).
#[test]
fn task_output_chunk_round_trips() {
    let ev = ClientEvent::TaskOutputChunk {
        task_id: "b1234abcd".to_string(),
        content: "line one\nline two\n".to_string(),
        total_lines: 2,
        truncated: false,
    };
    let json = serde_json::to_value(&ev).expect("serialize TaskOutputChunk");
    assert_eq!(json["type"], "task_output_chunk");
    assert_eq!(json["task_id"], "b1234abcd");
    assert_eq!(json["total_lines"], 2);
    assert_eq!(json["truncated"], false);
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize TaskOutputChunk");
    assert_eq!(back, ev);
}

/// `TaskStatusChanged` — a push event when a task transitions state.
#[test]
fn task_status_changed_round_trips() {
    let ev = ClientEvent::TaskStatusChanged {
        task_id: "b1234abcd".to_string(),
        status: TaskStatusDto::Completed,
        origin_session_id: None,
        error: None,
    };
    let json = serde_json::to_value(&ev).expect("serialize TaskStatusChanged");
    assert_eq!(json["type"], "task_status_changed");
    assert_eq!(json["task_id"], "b1234abcd");
    assert_eq!(json["status"]["type"], "completed");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize TaskStatusChanged");
    assert_eq!(back, ev);
}

/// All five task-status wire strings round-trip with `snake_case` tags (mirrors
/// `tasks::TaskStatus`, the 5 byte-locked statuses).
#[test]
fn task_status_variants_round_trip() {
    for (status, tag) in [
        (TaskStatusDto::Pending, "pending"),
        (TaskStatusDto::Running, "running"),
        (TaskStatusDto::Paused, "paused"),
        (TaskStatusDto::Completed, "completed"),
        (TaskStatusDto::Failed, "failed"),
        (TaskStatusDto::Cancelled, "cancelled"),
    ] {
        // `TaskStatusDto` is `Copy`, so pass by value (no needless borrow).
        let json = serde_json::to_value(status).expect("serialize TaskStatusDto");
        assert_eq!(json["type"], tag, "TaskStatusDto::{status:?} tag mismatch");
        let back: TaskStatusDto = serde_json::from_value(json).expect("deserialize TaskStatusDto");
        assert_eq!(back, status);
    }
}

// ── Coordinator (RESERVED — decision §0.9) ───────────────────────────────────

/// `CoordinatorStatus` — **RESERVED / feed-deferred** (decision §0.9):
/// coordinator/team is BLOCKED ON ENGINE WIRING (no `TeamRegistry` is
/// constructed in any assembled runtime). Present so the contract freezes now,
/// but MUST NOT be wired to a live source in the foundation. Round-trip only.
#[test]
fn coordinator_status_round_trips_reserved() {
    let ev = ClientEvent::CoordinatorStatus {
        active_workers: 0,
        team: None,
    };
    let json = serde_json::to_value(&ev).expect("serialize CoordinatorStatus");
    assert_eq!(json["type"], "coordinator_status");
    assert_eq!(json["active_workers"], 0);
    // None team is skipped on the wire.
    assert!(json.get("team").is_none(), "None team must be skipped");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize CoordinatorStatus");
    assert_eq!(back, ev);
}

/// `CoordinatorWorkerDto` — one per-worker roster row (T18). Field-shaped to
/// lower 1:1 onto the TUI `WorkerRow` (`agent_id` / `name` / `agent_type` /
/// `status`). The simplified `status` is a plain label string (the same
/// convention as `TaskRecord.status`).
#[test]
fn coordinator_worker_dto_roundtrip() {
    let dto = CoordinatorWorkerDto {
        agent_id: "agent:00000000-0000-0000-0000-000000000001".to_string(),
        name: "alpha".to_string(),
        agent_type: "explorer".to_string(),
        status: "working".to_string(),
    };
    let json = serde_json::to_value(&dto).expect("serialize CoordinatorWorkerDto");
    assert_eq!(
        json["agent_id"],
        "agent:00000000-0000-0000-0000-000000000001"
    );
    assert_eq!(json["name"], "alpha");
    assert_eq!(json["agent_type"], "explorer");
    assert_eq!(json["status"], "working");
    let back: CoordinatorWorkerDto =
        serde_json::from_value(json).expect("deserialize CoordinatorWorkerDto");
    assert_eq!(back, dto);
}

/// `CoordinatorWorker` — one roster row carried by an event (mirrors `TaskRow`).
/// This is the wire carrier the bridge PULL path (T19) emits one-per-worker.
#[test]
fn coordinator_worker_event_round_trips() {
    let ev = ClientEvent::CoordinatorWorker {
        worker: CoordinatorWorkerDto {
            agent_id: "agent:00000000-0000-0000-0000-000000000001".to_string(),
            name: "alpha".to_string(),
            agent_type: "explorer".to_string(),
            status: "working".to_string(),
        },
    };
    let json = serde_json::to_value(&ev).expect("serialize CoordinatorWorker");
    assert_eq!(json["type"], "coordinator_worker");
    assert_eq!(
        json["worker"]["agent_id"],
        "agent:00000000-0000-0000-0000-000000000001"
    );
    assert_eq!(json["worker"]["agent_type"], "explorer");
    assert_eq!(json["worker"]["status"], "working");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize CoordinatorWorker");
    assert_eq!(back, ev);
}
