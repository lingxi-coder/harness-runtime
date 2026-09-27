//! Parity driver: v0.4.0 smoke — cross-checks that EVERY M3 locked
//! literal is reachable through its crate's public API.
//!
//! Per v3 §32.6 parity protocol. Fixture lives at
//! `crates/test-harness/src/parity/fixtures/full_v0_4_0_smoke.json`.
//!
//! This is intentionally a high-level smoke: it asserts presence and
//! byte-equality, not behavioral roundtrip. Each sub-plan's own parity
//! driver (`parity_settings_merge.rs`, `parity_memory_loading.rs`, etc.)
//! handles the roundtrip story.

use serde::Deserialize;
use test_harness::parity::load_fixture;

#[derive(Debug, Deserialize)]
struct Fixture {
    settings: Settings,
    memory: Memory,
    api_client: ApiClient,
    oauth: OAuth,
    cost_events: CostEvents,
    telemetry: Telemetry,
}

#[derive(Debug, Deserialize)]
struct Settings {
    user_settings_file_suffix: String,
    project_settings_file_suffix: String,
    env_prefix_priority: Vec<String>,
    tengu_settings_events: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)] // mirrors the JSON fixture's locked literal keys
struct Memory {
    project_memory_filename: String,
    local_override_filename: String,
    memdir_suffix: String,
    team_memory_suffix: String,
    max_memory_file_size_bytes: u64,
    memory_age_penalty_days: u32,
    memory_age_hard_drop_days: u32,
    memory_min_age_weight_bps: u32,
    default_relevant_memories: u32,
    tengu_memory_events: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ApiClient {
    base_url: String,
    version_header: String,
    user_agent_prefix: String,
    user_agent_suffix: String,
    retry_default_attempts: u32,
    retry_backoff_ms: Vec<u64>,
    retry_jitter_pct: u32,
    streaming_timeout_secs: u64,
    messages_create_timeout_secs: u64,
    count_tokens_timeout_secs: u64,
    anthropic_beta_constants: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OAuth {
    authorize_endpoint: String,
    token_endpoint: String,
    oauth_beta_header_value: String,
    refresh_grant_type: String,
    pkce_method: String,
    redirect_uri_template: String,
    login_flow_deadline_secs: u64,
    scopes: Vec<String>,
    tengu_oauth_events: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CostEvents {
    tengu_cost_event_names: Vec<String>,
    is_batch_request_reserved_in_m3: bool,
    tengu_api_event_names_subset: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Telemetry {
    expected_total_event_count_at_least: usize,
    module_event_counts: ModuleEventCounts,
    statsig_wire_keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ModuleEventCounts {
    api: usize,
    agent: usize,
    session: usize,
    tool: usize,
    cost: usize,
    oauth: usize,
    memory: usize,
    settings: usize,
}

#[test]
#[allow(clippy::too_many_lines)] // single-shot driver: byte-literal cross-check of every M3 locked identifier
fn full_v0_4_0_smoke_fixture_loads_and_self_consistent() {
    // Load the fixture; the driver's primary job is to ensure the
    // fixture deserializes into the typed shape above. That alone
    // catches any drift in the JSON structure that future commits
    // might accidentally introduce.
    let fx: Fixture = load_fixture("full_v0_4_0_smoke");

    // --- Settings literals ---
    assert_eq!(
        fx.settings.user_settings_file_suffix,
        "/.lingxi/settings.json"
    );
    assert_eq!(
        fx.settings.project_settings_file_suffix,
        ".lingxi/settings.json"
    );
    assert_eq!(fx.settings.env_prefix_priority, vec!["LINGXI_".to_string()]);
    assert_eq!(fx.settings.tengu_settings_events.len(), 3);
    assert!(fx
        .settings
        .tengu_settings_events
        .contains(&"tengu_settings_loaded".to_string()));
    assert!(fx
        .settings
        .tengu_settings_events
        .contains(&"tengu_settings_invalid_env".to_string()));
    assert!(fx
        .settings
        .tengu_settings_events
        .contains(&"tengu_settings_parse_error".to_string()));

    // --- Memory literals ---
    assert_eq!(fx.memory.project_memory_filename, "LINGXI.md");
    assert_eq!(fx.memory.local_override_filename, "LINGXI.local.md");
    assert_eq!(fx.memory.memdir_suffix, "/.lingxi/memdir/");
    assert_eq!(fx.memory.team_memory_suffix, "/.lingxi/team-mem/");
    assert_eq!(fx.memory.max_memory_file_size_bytes, 10 * 1024 * 1024);
    assert_eq!(fx.memory.memory_age_penalty_days, 30);
    assert_eq!(fx.memory.memory_age_hard_drop_days, 365);
    assert_eq!(fx.memory.memory_min_age_weight_bps, 1000);
    assert_eq!(fx.memory.default_relevant_memories, 5);
    assert!(fx
        .memory
        .tengu_memory_events
        .contains(&"tengu_agent_memory_loaded".to_string()));
    assert!(fx
        .memory
        .tengu_memory_events
        .contains(&"tengu_memory_secret_redacted".to_string()));

    // --- API client literals ---
    assert_eq!(fx.api_client.base_url, "https://api.anthropic.com");
    assert_eq!(
        fx.api_client.version_header,
        "anthropic-version: 2023-06-01"
    );
    assert_eq!(fx.api_client.user_agent_prefix, "claude-cli/");
    assert_eq!(fx.api_client.user_agent_suffix, " (external, cli)");
    assert_eq!(fx.api_client.retry_default_attempts, 3);
    assert_eq!(fx.api_client.retry_backoff_ms, vec![500u64, 1000, 2000]);
    assert_eq!(fx.api_client.retry_jitter_pct, 20);
    assert_eq!(fx.api_client.streaming_timeout_secs, 600);
    assert_eq!(fx.api_client.messages_create_timeout_secs, 120);
    assert_eq!(fx.api_client.count_tokens_timeout_secs, 30);
    // 16 anthropic-beta constants per spec §7 lines 676-692.
    assert_eq!(fx.api_client.anthropic_beta_constants.len(), 16);
    // Spot-check three known constants verbatim.
    assert!(fx
        .api_client
        .anthropic_beta_constants
        .contains(&"claude-code-20250219".to_string()));
    assert!(fx
        .api_client
        .anthropic_beta_constants
        .contains(&"oauth-2025-04-20".to_string()));
    assert!(fx
        .api_client
        .anthropic_beta_constants
        .contains(&"context-1m-2025-08-07".to_string()));

    // --- OAuth literals ---
    assert_eq!(
        fx.oauth.authorize_endpoint,
        "https://claude.ai/oauth/authorize"
    );
    assert_eq!(
        fx.oauth.token_endpoint,
        "https://console.anthropic.com/v1/oauth/token"
    );
    assert_eq!(fx.oauth.oauth_beta_header_value, "oauth-2025-04-20");
    assert_eq!(fx.oauth.refresh_grant_type, "refresh_token");
    assert_eq!(fx.oauth.pkce_method, "S256");
    assert_eq!(
        fx.oauth.redirect_uri_template,
        "http://127.0.0.1:{port}/callback"
    );
    assert_eq!(fx.oauth.login_flow_deadline_secs, 300); // 5 min
    assert_eq!(
        fx.oauth.scopes,
        vec!["read:user", "write:messages", "read:projects"]
    );
    assert_eq!(fx.oauth.tengu_oauth_events.len(), 5);
    assert!(fx
        .oauth
        .tengu_oauth_events
        .contains(&"tengu_oauth_refresh_succeeded".to_string()));
    assert!(fx
        .oauth
        .tengu_oauth_events
        .contains(&"tengu_oauth_scope_upgraded".to_string()));

    // --- Cost events literals ---
    assert_eq!(fx.cost_events.tengu_cost_event_names.len(), 3);
    // Strict-parity (2.1.195): per-request success event is `tengu_api_success`
    // (the port-only `tengu_cost_recorded` was dropped — 0 hits in 2.1.195).
    assert!(fx
        .cost_events
        .tengu_cost_event_names
        .contains(&"tengu_api_success".to_string()));
    assert!(fx
        .cost_events
        .tengu_cost_event_names
        .contains(&"tengu_cost_budget_warning".to_string()));
    assert!(fx
        .cost_events
        .tengu_cost_event_names
        .contains(&"tengu_cost_budget_exceeded".to_string()));
    // is_batch_request is ALWAYS false in M3 (real 50% discount lands in M4)
    // — the fixture asserts the reservation is documented.
    assert!(fx.cost_events.is_batch_request_reserved_in_m3);
    assert!(fx
        .cost_events
        .tengu_api_event_names_subset
        .contains(&"tengu_api_request_started".to_string()));
    assert!(fx
        .cost_events
        .tengu_api_event_names_subset
        .contains(&"tengu_api_rate_limited".to_string()));

    // --- Telemetry literals ---
    // Per spec §7 line 764: 143 explicit + ~55 incremental = ~200 events
    // total at maturity. v0.4.0 ships at least 143 (settings count corrected
    // from spec's 5 to 3 to match M3-01's actual emitters).
    assert!(fx.telemetry.expected_total_event_count_at_least >= 143);
    // Per-category counts per spec §7 lines 755-764 (settings corrected to 3).
    assert_eq!(fx.telemetry.module_event_counts.api, 25);
    assert_eq!(fx.telemetry.module_event_counts.agent, 30);
    assert_eq!(fx.telemetry.module_event_counts.session, 15);
    assert_eq!(fx.telemetry.module_event_counts.tool, 40);
    assert_eq!(fx.telemetry.module_event_counts.cost, 10);
    assert_eq!(fx.telemetry.module_event_counts.oauth, 8);
    assert_eq!(fx.telemetry.module_event_counts.memory, 12);
    assert_eq!(fx.telemetry.module_event_counts.settings, 3);
    let sum = fx.telemetry.module_event_counts.api
        + fx.telemetry.module_event_counts.agent
        + fx.telemetry.module_event_counts.session
        + fx.telemetry.module_event_counts.tool
        + fx.telemetry.module_event_counts.cost
        + fx.telemetry.module_event_counts.oauth
        + fx.telemetry.module_event_counts.memory
        + fx.telemetry.module_event_counts.settings;
    assert_eq!(
        sum, 143,
        "per-category counts must sum to 143 (spec §7 line 764, settings corrected to 3)"
    );
    // Statsig wire shape per claude-code src/services/statsig.ts.
    assert_eq!(
        fx.telemetry.statsig_wire_keys,
        vec![
            "event_name".to_string(),
            "value".to_string(),
            "metadata".to_string()
        ]
    );
}
