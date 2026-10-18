use super::authentication::spec_url;
#[cfg(test)]
use super::record_test_telemetry_event;
use super::{McpCatalogChanged, McpCatalogKind};
use crate::connection::{ConfigScope, McpServerConfig};
use crate::oauth;
use lingxi_core::host::McpError;
use lingxi_core::types::McpConnectionId;
use std::sync::OnceLock;
use tokio::sync::broadcast;

/// Sanitize one raw text value through Claude Code 2.1.295's `nl` helper.
/// This is intentionally separate from [`sanitize_diagnostic`], which ports
/// `xLt` for ToolSearch notes and does not redact credential-shaped text.
pub(super) fn sanitize_native_mcp_text(input: &str) -> String {
    use unicode_normalization::UnicodeNormalization;

    const INPUT_UTF16_LIMIT: usize = 4096;
    const OUTPUT_LIMIT: usize = 200;
    const REDACTION_LOOKAHEAD: usize = 512;
    const SAFE_INPUT_FLOOR: usize = 2000;

    // Native evaluates nko(CZ(text)): NFKC happens before the 4,096 UTF-16
    // code-unit cap, and nko replaces controls, format characters, default
    // ignorables, line separators, and non-ASCII space separators.
    let normalized: String = input.nfkc().collect();
    let normalized = native_nko(&normalized, INPUT_UTF16_LIMIT);
    let safe_limit = SAFE_INPUT_FLOOR.max(OUTPUT_LIMIT + REDACTION_LOOKAHEAD);
    let was_truncated = utf16_len(&normalized) > safe_limit;
    let safe_prefix = if was_truncated {
        truncate_utf16(&normalized, safe_limit)
    } else {
        normalized
    };

    // l5t runs after delimiter/space normalization and before final clipping.
    let redacted = redact_native_mcp_secrets(&safe_prefix);
    if was_truncated || utf16_len(&redacted) > OUTPUT_LIMIT {
        format!("{}…", truncate_utf16(&redacted, OUTPUT_LIMIT))
    } else {
        redacted
    }
}

fn native_nko(input: &str, limit: usize) -> String {
    static REPLACED_CHARACTERS: OnceLock<regex::Regex> = OnceLock::new();
    let replaced_characters = REPLACED_CHARACTERS.get_or_init(|| {
        regex::Regex::new(
            r"^[\p{Cc}\p{Cf}\p{Zl}\p{Zp}\p{Default_Ignorable_Code_Point}\u2800\p{Zs}]$",
        )
        .expect("Native text character classes are valid")
    });

    let mut sanitized = String::with_capacity(input.len().min(limit));
    let mut units = 0;
    for character in input.chars() {
        let character_units = character.len_utf16();
        if units + character_units > limit {
            // Native first slices UTF-16 and replaces an unpaired surrogate
            // from a split pair with one space.
            if units < limit {
                sanitized.push(' ');
            }
            break;
        }
        units += character_units;
        if character != ' ' && replaced_characters.is_match(&character.to_string()) {
            sanitized.push(' ');
        } else {
            sanitized.push(character);
        }
    }

    let mut collapsed = String::with_capacity(sanitized.len());
    let mut pending_space = false;
    for character in sanitized.chars() {
        if native_nl_delimiter(character) || is_ecmascript_whitespace(character) {
            pending_space = !collapsed.is_empty();
            continue;
        }
        if pending_space {
            collapsed.push(' ');
            pending_space = false;
        }
        collapsed.push(character);
    }
    collapsed
}

fn is_ecmascript_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

fn native_nl_delimiter(character: char) -> bool {
    matches!(
        character,
        '<' | '>'
            | '"'
            | ';'
            | '\u{2018}'
            | '\u{2019}'
            | '\u{201A}'
            | '\u{201C}'
            | '\u{201D}'
            | '\u{201E}'
            | '\u{00AB}'
            | '\u{00BB}'
            | '\u{2039}'
            | '\u{203A}'
            | '\u{2329}'
            | '\u{232A}'
            | '\u{27E8}'
            | '\u{27E9}'
            | '\u{27EA}'
            | '\u{27EB}'
            | '\u{3008}'
            | '\u{3009}'
            | '\u{300A}'
            | '\u{300B}'
    )
}

fn utf16_len(input: &str) -> usize {
    input.encode_utf16().count()
}

fn truncate_utf16(input: &str, limit: usize) -> String {
    let mut units = 0;
    let mut end = 0;
    for (byte_index, character) in input.char_indices() {
        let character_units = character.len_utf16();
        if units + character_units > limit {
            break;
        }
        units += character_units;
        end = byte_index + character.len_utf8();
    }
    input[..end].to_owned()
}

fn redact_native_mcp_secrets(input: &str) -> String {
    static BEARER_SECRET: OnceLock<fancy_regex::Regex> = OnceLock::new();
    static LABELED_SECRET: OnceLock<fancy_regex::Regex> = OnceLock::new();

    const SECRET_LABEL: &str = r"(?:access[-_ ]?|refresh[-_ ]?|id[-_ ]?|client[-_ ]?|api[-_ ]?|x[-_]api[-_ ]?|session[-_ ]?|auth[-_ ]?)?(?:token|key|secret|password|authorization|credential)s?";

    let token_start = format!(
        r#"\b{SECRET_LABEL}(?:\\*["']|\+)?\s*[:=\uFF1A\uFF1D]\s*(?:\\*["']|\+)?\s*(?!=*{SECRET_LABEL}=*(?![A-Za-z0-9._~+/=%-]))(?=[A-Za-z0-9._~+/=%-]{{8}})[A-Za-z]*[0-9._~+/=%-]"#
    );
    let token_value =
        format!(r"(?:[A-Za-z0-9._~+/=%-]+?(?={token_start})|[A-Za-z0-9._~+/=%-]{{8,}})");
    let bearer_secret = BEARER_SECRET.get_or_init(|| {
        fancy_regex::RegexBuilder::new(&format!(
            r#"(?:\b|(?<=\\[A-Za-z"']))(bearer|basic)(?![\s:=\uFF1A\uFF1D]+(?:(?:\\*["']|\+)\s*)?{SECRET_LABEL}=*(?![A-Za-z0-9._~+/=%-]))[\s:=\uFF1A\uFF1D]+(?:(?:\\*["']|\+)\s*)?({token_value})"#
        ))
        .case_insensitive(true)
        .unicode_mode(false)
        .build()
        .expect("Native bearer credential expression is valid")
    });
    let labeled_secret = LABELED_SECRET.get_or_init(|| {
        fancy_regex::RegexBuilder::new(&format!(
            r#"(?:\b|(?<=\\[A-Za-z"']))({SECRET_LABEL})(?:\\*["']|\+)?\s*[:=\uFF1A\uFF1D]\s*(?:\\*["']|\+)?\s*(?!=*{SECRET_LABEL}=*(?![A-Za-z0-9._~+/=%-]))({token_value})"#
        ))
        .case_insensitive(true)
        .unicode_mode(false)
        .build()
        .expect("Native labeled credential expression is valid")
    });

    let input = redact_matches(input, bearer_secret);
    redact_matches(&input, labeled_secret)
}

fn redact_matches(input: &str, expression: &fancy_regex::Regex) -> String {
    let mut output = String::with_capacity(input.len());
    let mut copied_through = 0;
    for capture in expression.captures_iter(input) {
        let Ok(capture) = capture else {
            // The source regex has no explicit backtracking limit. If the
            // compatible regex engine reaches its default execution bound,
            // preserve the conservative redaction result.
            return "[redacted]".to_owned();
        };
        let Some(matched) = capture.get(0) else {
            continue;
        };
        let Some(label) = capture.get(1) else {
            continue;
        };
        let Some(secret) = capture.get(2) else {
            continue;
        };
        if !secret.as_str().chars().any(native_secret_marker) {
            continue;
        }

        output.push_str(&input[copied_through..matched.start()]);
        output.push_str(label.as_str());
        output.push_str(" [redacted]");
        if input[matched.end()..]
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            output.push(' ');
        }
        copied_through = matched.end();
    }
    output.push_str(&input[copied_through..]);
    output
}

fn native_secret_marker(character: char) -> bool {
    character.is_ascii_digit() || matches!(character, '.' | '_' | '~' | '+' | '/' | '=' | '%' | '-')
}

pub(super) fn sanitize_native_mcp_projection(
    value: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    let text = value
        .string_units("")
        .map(|units| decode_utf16_invalid_as_spaces(&units))
        .map(|text| sanitize_native_mcp_text(&text))
        .unwrap_or_default();
    Utf16JsonProjection::root_string(text.clone(), text.encode_utf16().collect())
        .expect("sanitized failure text is valid UTF-16")
}

fn decode_utf16_invalid_as_spaces(units: &[u16]) -> String {
    let mut text = String::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        if (0xD800..=0xDBFF).contains(&unit) {
            if let Some(low) = units
                .get(index + 1)
                .copied()
                .filter(|low| (0xDC00..=0xDFFF).contains(low))
            {
                let scalar =
                    0x10000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
                text.push(char::from_u32(scalar).expect("valid UTF-16 pair"));
                index += 2;
            } else {
                text.push(' ');
                index += 1;
            }
        } else if (0xDC00..=0xDFFF).contains(&unit) {
            text.push(' ');
            index += 1;
        } else {
            text.push(char::from_u32(u32::from(unit)).expect("non-surrogate UTF-16 unit"));
            index += 1;
        }
    }
    text
}

/// Sanitize a model-visible diagnostic string (an MCP server name or failure
/// message) — a 1:1 port of claude-code's `xLt`, used to build the `ToolSearch`
/// empty-result failed-server note. Steps mirror claude exactly:
///
/// 1. NFKC-normalize.
/// 2. Replace control (`\p{Cc}`) / format (`\p{Cf}`) characters and
///    U+2028/U+2029 with a space (claude's `qU`).
/// 3. Replace angle brackets, `"`, `;` and a set of fancy quote / bracket
///    characters with a space.
/// 4. Collapse runs of whitespace to a single space and trim.
/// 5. Truncate to 200 characters, appending `…` (U+2026) when it was longer.
///
/// Steps 2–4 all map their targets to a space and then collapse, so a single
/// pass that treats every stripped-or-whitespace character as a collapsing
/// space produces the same result.
pub(super) fn sanitize_diagnostic(input: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    /// claude's `RJu`.
    const LIMIT: usize = 200;

    // The explicit character class claude replaces with a space (step 3).
    fn is_stripped(c: char) -> bool {
        matches!(
            c,
            '<' | '>'
                | '"'
                | ';'
                | '\u{2018}'
                | '\u{2019}'
                | '\u{201A}'
                | '\u{201C}'
                | '\u{201D}'
                | '\u{201E}'
                | '\u{00AB}'
                | '\u{00BB}'
                | '\u{2039}'
                | '\u{203A}'
                | '\u{2329}'
                | '\u{232A}'
                | '\u{27E8}'
                | '\u{27E9}'
                | '\u{27EA}'
                | '\u{27EB}'
                | '\u{3008}'
                | '\u{3009}'
                | '\u{300A}'
                | '\u{300B}'
        )
    }

    let normalized: String = input.nfkc().collect();
    let mut collapsed = String::with_capacity(normalized.len());
    let mut pending_space = false;
    for c in normalized.chars() {
        // Step 2 (`qU`): control / format chars + U+2028/U+2029, step 3's
        // explicit class, and any other whitespace (step 4's `\s+`) all become
        // collapsing spaces.
        let is_control_or_format =
            c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') || is_format_char(c);
        if is_control_or_format || is_stripped(c) || c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !collapsed.is_empty() {
            collapsed.push(' ');
        }
        pending_space = false;
        collapsed.push(c);
    }
    // `collapsed` already has no leading/interior double spaces or trailing
    // space (pending_space is dropped at end), i.e. it is already trimmed.
    if collapsed.chars().count() > LIMIT {
        let head: String = collapsed.chars().take(LIMIT).collect();
        format!("{head}\u{2026}")
    } else {
        collapsed
    }
}

/// Whether `c` is a Unicode format character (general category `Cf`) — the
/// portion of claude's `qU` (`\p{Cf}`) not covered by [`char::is_control`]
/// (which is `Cc`). Enumerates the stable `Cf` code-point blocks.
pub(super) fn is_format_char(c: char) -> bool {
    let cp = c as u32;
    matches!(
        cp,
        0x00AD              // SOFT HYPHEN
        | 0x0600..=0x0605   // Arabic number signs
        | 0x061C            // Arabic Letter Mark
        | 0x06DD            // Arabic End of Ayah
        | 0x070F            // Syriac Abbreviation Mark
        | 0x0890..=0x0891   // Arabic pound / piastre marks
        | 0x08E2            // Arabic disputed end of ayah
        | 0x180E            // Mongolian vowel separator
        | 0x200B..=0x200F   // zero-width + LTR/RTL marks
        | 0x202A..=0x202E   // directional formatting
        | 0x2060..=0x2064   // word joiner + invisible operators
        | 0x2066..=0x206F   // directionality + deprecated
        | 0xFEFF            // ZERO WIDTH NO-BREAK SPACE / BOM
        | 0xFFF9..=0xFFFB   // interlinear annotation
        | 0x110BD           // Kaithi number sign
        | 0x110CD           // Kaithi number sign above
        | 0x13430..=0x1343F // Egyptian Hieroglyph format controls
        | 0x1BCA0..=0x1BCA3 // Shorthand format controls
        | 0x1D173..=0x1D17A // Musical symbol begin/end
        | 0xE0001           // Language tag
        | 0xE0020..=0xE007F // Tags block
    )
}

/// §20b — `tengu_mcp_server_config_invalid`: a server's config failed the
/// loader-time or connect-time URL/shape re-validation. Oracle call site:
/// `s("tengu_mcp_server_config_invalid",{transportType:c(t.type??"stdio"),
/// field:w("url"),source:w(t.configError?"loader":"connect")})` — `field` is
/// always the literal `"url"`, the sole re-validation target either gate
/// checks (see [`McpServerConfig::config_error`] /
/// [`McpServerConfig::connect_time_url_error`]'s docs for the two gates this
/// fires from).
/// §20b — build `tengu_mcp_tools_listed`'s payload. Pure: takes the already
/// resolved/filtered tool list and elapsed duration rather than reaching
/// into `self`/`conn`, so the field-mapping (`tool_count`/`always_load_count`
/// off the FINAL post-§20a-filter list, not the raw transport response) is
/// unit-testable without standing up a mock transport.
///
/// `discovery_source` is unconditionally `"live"` here — this is the
/// CONNECT path (a fresh dial), never the cached-row-adoption path the
/// oracle's `discoverySource` also covers (§18's deferred
/// `cached-row adopt subscriber threw` item; this port has no cached-row
/// adoption at all yet).
/// Oracle `yn`'s FIRST statement (@182316780):
/// `if(u.length===0&&r==="live")s("tengu_mcp_degraded",{reason:w("connected_zero_tools"),…})`.
///
/// Pure and separate from the `connect` body so the three conditions are
/// unit-testable without standing up a transport:
///
/// * `u.length === 0` — `u` is the RAW `tools/list` response, so emptiness is
///   measured BEFORE the §20a schema filter runs. A server whose every tool
///   that filter dropped reports its drop reason, NOT zero-tools.
/// * `r === "live"` — always true on this path (a fresh dial); the oracle's
///   cached-row adoption path, which also feeds `yn`, does not exist here.
/// * `caps.tools` — with no tools capability the oracle never reaches `yn`,
///   so an empty list there is not a degraded signal (same gate as
///   `tengu_mcp_tools_listed`).
pub(super) fn connected_zero_tools_fires(caps_tools: bool, raw_tool_count: usize) -> bool {
    caps_tools && raw_tool_count == 0
}

/// §11 — pure core of `McpRegistry::connect_locked_inner`'s MISS branch:
/// given the pre-dial [`crate::discovery_cache::Decision`], what `source`
/// string to emit on `tengu_mcp_discovery_source` (if anything). `None`
/// means do not emit at all — either a `Fresh`/`Stale` decision (a HIT is
/// handled entirely separately by `McpRegistry::serve_discovery_cache_hit`,
/// which emits its own `"cache_fresh"`/`"cache_stale"` — this function is
/// never even called for one), or a `Miss` reason the oracle's `Ko` gate
/// excludes (`Disabled`/`Transport`/`LiveConnection`/`SkillsCapable`/
/// `ChannelCapable`).
pub(super) fn discovery_source_emission(
    decision: &crate::discovery_cache::Decision,
) -> Option<&'static str> {
    match decision {
        crate::discovery_cache::Decision::Miss { reason }
            if crate::discovery_cache::miss_emits_discovery_source_telemetry(*reason) =>
        {
            Some(crate::discovery_cache::miss_telemetry_value(*reason))
        }
        _ => None,
    }
}

pub(super) fn tools_listed_payload(
    transport_kind: &str,
    elapsed: std::time::Duration,
    tools: &[lingxi_core::host::McpToolDto],
    server_name: &str,
) -> telemetry::tengu::mcp::ToolsListedPayload {
    use telemetry::pii::Verified;
    telemetry::tengu::mcp::ToolsListedPayload {
        transport_type: Verified::assert_safe(transport_kind.to_string()),
        list_duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        tool_count: u32::try_from(tools.len()).unwrap_or(u32::MAX),
        always_load_count: u32::try_from(
            tools.iter().filter(|t| t.always_load == Some(true)).count(),
        )
        .unwrap_or(u32::MAX),
        discovery_source: Verified::assert_safe("live".to_string()),
        // Oracle: `mcpServerName:EA(ln(e.name),HT(e.name,e.config))` — `EA`
        // returns `undefined` (the spread DROPS the key) unless the
        // first-party gate holds. Emitting the raw name unconditionally, as
        // an earlier revision did, made a user's private server name an
        // analytics dimension on every connect. See
        // `telemetry::tengu::mcp::server_name_gate`.
        mcp_server_name: telemetry::tengu::mcp::server_name_gate(transport_kind)
            .then(|| Verified::assert_safe(server_name.to_string())),
    }
}

pub(super) fn emit_server_config_invalid(
    config: &McpServerConfig,
    source: telemetry::tengu::mcp::ConfigInvalidSource,
) {
    telemetry::emit_mcp_server_config_invalid(&server_config_invalid_payload(config, source));
}

/// Pure payload-building half of [`emit_server_config_invalid`] — split out
/// so the loader-vs-connect classification is unit-testable directly,
/// without a tracing-capture race (see `degraded_payloads_for_server`'s doc
/// for why that race is real in this shared test binary).
pub(super) fn server_config_invalid_payload(
    config: &McpServerConfig,
    source: telemetry::tengu::mcp::ConfigInvalidSource,
) -> telemetry::tengu::mcp::ServerConfigInvalidPayload {
    use telemetry::pii::Verified;
    telemetry::tengu::mcp::ServerConfigInvalidPayload {
        transport_type: Verified::assert_safe(config.spec.kind().to_string()),
        field: Verified::assert_safe("url".to_string()),
        source,
    }
}

pub(super) fn config_scope_wire(scope: ConfigScope) -> &'static str {
    match scope {
        ConfigScope::Settings(lingxi_core::types::SettingsScope::Local) => "local",
        ConfigScope::Settings(lingxi_core::types::SettingsScope::User) => "user",
        ConfigScope::Settings(lingxi_core::types::SettingsScope::Project) => "project",
        ConfigScope::Dynamic => "dynamic",
        ConfigScope::Enterprise => "enterprise",
        ConfigScope::ClaudeAi => "claudeai",
        ConfigScope::Settings(lingxi_core::types::SettingsScope::Managed) => "managed",
        ConfigScope::Agent => "agent",
    }
}

pub(super) fn negotiation_mode_wire(
    negotiation_mode: crate::protocol_negotiation::NegotiationMode,
) -> &'static str {
    match negotiation_mode {
        crate::protocol_negotiation::NegotiationMode::Legacy => "legacy",
        crate::protocol_negotiation::NegotiationMode::Auto { .. } => "auto",
    }
}

pub(super) fn protocol_era_wire(era: lingxi_core::host::McpProtocolEra) -> &'static str {
    match era {
        lingxi_core::host::McpProtocolEra::Legacy => "legacy",
        lingxi_core::host::McpProtocolEra::Modern => "modern",
    }
}

pub(super) fn is_plugin_mcp_config(config: &McpServerConfig) -> bool {
    matches!(
        config.metadata.agent_source,
        Some(crate::connection::McpAgentSource::Plugin)
    )
}

pub(super) fn server_connection_succeeded_payload(
    config: &McpServerConfig,
    connection_duration_ms: u64,
    negotiation_mode: crate::protocol_negotiation::NegotiationMode,
    negotiated: &lingxi_core::host::McpNegotiatedProtocol,
) -> telemetry::tengu::mcp::ServerConnectionSucceededPayload {
    use telemetry::pii::Verified;
    telemetry::tengu::mcp::ServerConnectionSucceededPayload {
        connection_duration_ms,
        transport_type: Verified::assert_safe(config.spec.kind().to_string()),
        scope: Verified::assert_safe(config_scope_wire(config.scope).to_string()),
        is_plugin: is_plugin_mcp_config(config),
        negotiation_mode: Some(Verified::assert_safe(
            negotiation_mode_wire(negotiation_mode).to_string(),
        )),
        protocol_era: Some(Verified::assert_safe(
            protocol_era_wire(negotiated.era).to_string(),
        )),
        negotiated_protocol_version: Some(Verified::assert_safe(negotiated.version.clone())),
    }
}

pub(super) fn server_connection_failed_payload(
    config: &McpServerConfig,
    negotiation_mode: Option<crate::protocol_negotiation::NegotiationMode>,
    connection_duration_ms: Option<u64>,
    error_code: Option<&'static str>,
) -> telemetry::tengu::mcp::ServerConnectionFailedPayload {
    use telemetry::pii::Verified;
    telemetry::tengu::mcp::ServerConnectionFailedPayload {
        transport_type: Verified::assert_safe(config.spec.kind().to_string()),
        scope: Verified::assert_safe(config_scope_wire(config.scope).to_string()),
        is_plugin: is_plugin_mcp_config(config),
        connection_duration_ms,
        negotiation_mode: negotiation_mode
            .map(negotiation_mode_wire)
            .map(|mode| Verified::assert_safe(mode.to_string())),
        error_code: error_code.map(|code| Verified::assert_safe(code.to_string())),
    }
}

pub(super) fn list_changed_payload(
    server_name: &str,
    kind: McpCatalogKind,
    cause: &'static str,
    previous_count: Option<usize>,
    new_count: Option<usize>,
) -> telemetry::tengu::mcp::ListChangedPayload {
    use telemetry::pii::Verified;
    telemetry::tengu::mcp::ListChangedPayload {
        kind: match kind {
            McpCatalogKind::Tools => telemetry::tengu::mcp::ListChangedType::Tools,
            McpCatalogKind::Prompts => telemetry::tengu::mcp::ListChangedType::Prompts,
            McpCatalogKind::Resources => telemetry::tengu::mcp::ListChangedType::Resources,
        },
        mcp_server_key_hash: mcp_server_key_hash(server_name),
        cause: Verified::assert_safe(cause.to_string()),
        previous_count: previous_count
            .map(|count| u32::try_from(count).unwrap_or(u32::MAX))
            .filter(|_| kind == McpCatalogKind::Tools),
        new_count: new_count
            .map(|count| u32::try_from(count).unwrap_or(u32::MAX))
            .filter(|_| kind == McpCatalogKind::Tools),
    }
}

pub(super) fn resource_templates_fetched_payload(
    templates: &[lingxi_core::host::McpResourceTemplateDto],
) -> telemetry::tengu::mcp::ResourceTemplatesFetchedPayload {
    telemetry::tengu::mcp::ResourceTemplatesFetchedPayload {
        template_count: u32::try_from(templates.len()).unwrap_or(u32::MAX),
    }
}

pub(super) fn mcp_server_key_hash(server_name: &str) -> telemetry::pii::Verified {
    oauth::telemetry_server_key_hash_for_key(server_name)
}

pub(super) fn emit_oauth_flow_failure(payload: &telemetry::tengu::mcp::OAuthFlowFailurePayload) {
    tracing::info!(
        event = telemetry::tengu::mcp::OAUTH_FLOW_FAILURE,
        authMethod = payload.auth_method.as_str(),
        xaaFailureStage = payload.xaa_failure_stage.as_str(),
        idTokenCacheHit = payload.id_token_cache_hit,
    );
    let attrs = std::iter::IntoIterator::into_iter([
        (
            "authMethod".to_string(),
            telemetry::otel::AttrValue::from(payload.auth_method.as_str().to_string()),
        ),
        (
            "xaaFailureStage".to_string(),
            telemetry::otel::AttrValue::from(payload.xaa_failure_stage.as_str().to_string()),
        ),
        (
            "idTokenCacheHit".to_string(),
            telemetry::otel::AttrValue::from(payload.id_token_cache_hit),
        ),
    ])
    .collect();
    telemetry::otel::emit_named_log_event(telemetry::tengu::mcp::OAUTH_FLOW_FAILURE, &attrs);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::OAUTH_FLOW_FAILURE,
        serde_json::to_value(payload).expect("serialize test oauth_flow_failure payload"),
    );
}

pub(super) fn emit_xaa_oauth_flow_success(
    payload: &telemetry::tengu::mcp::OAuthXaaFlowSuccessPayload,
) {
    tracing::info!(
        event = telemetry::tengu::mcp::OAUTH_FLOW_SUCCESS,
        authMethod = payload.auth_method.as_str(),
        idTokenCacheHit = payload.id_token_cache_hit,
    );
    let attrs = std::iter::IntoIterator::into_iter([
        (
            "authMethod".to_string(),
            telemetry::otel::AttrValue::from(payload.auth_method.as_str().to_string()),
        ),
        (
            "idTokenCacheHit".to_string(),
            telemetry::otel::AttrValue::from(payload.id_token_cache_hit),
        ),
    ])
    .collect();
    telemetry::otel::emit_named_log_event(telemetry::tengu::mcp::OAUTH_FLOW_SUCCESS, &attrs);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::OAUTH_FLOW_SUCCESS,
        serde_json::to_value(payload).expect("serialize test XAA oauth_flow_success payload"),
    );
}

pub(super) fn telemetry_mcp_server_base_url(
    spec: &lingxi_core::host::McpTransportSpec,
) -> Option<telemetry::Verified> {
    let mut url = url::Url::parse(spec_url(spec)).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    let normalized = url.to_string();
    let normalized = normalized.strip_suffix('/').unwrap_or(normalized.as_str());
    // Oracle `gg(IAe(config))` hashes the credential/query/fragment-free URL
    // with SHA-256 and keeps the first 12 hex digits. The misleading
    // `mcpServerBaseUrl` field name must not cause the normalized URL itself to
    // leave the process.
    Some(oauth::telemetry_server_key_hash_for_key(normalized))
}

pub(super) fn emit_session_expired(payload: &telemetry::tengu::mcp::SessionExpiredPayload) {
    match payload.error_code.as_ref() {
        Some(error_code) => tracing::info!(
            event = telemetry::tengu::mcp::SESSION_EXPIRED,
            errorCode = error_code.as_str(),
            transportType = payload.transport_type.as_str(),
            mcpServerKeyHash = payload.mcp_server_key_hash.as_str(),
            mcpServerBaseUrl = payload
                .mcp_server_base_url
                .as_ref()
                .map(telemetry::Verified::as_str),
        ),
        None => tracing::info!(
            event = telemetry::tengu::mcp::SESSION_EXPIRED,
            transportType = payload.transport_type.as_str(),
            mcpServerKeyHash = payload.mcp_server_key_hash.as_str(),
            mcpServerBaseUrl = payload
                .mcp_server_base_url
                .as_ref()
                .map(telemetry::Verified::as_str),
        ),
    }
    let mut attrs = std::collections::BTreeMap::from([
        (
            "transportType".to_string(),
            telemetry::otel::AttrValue::from(payload.transport_type.as_str().to_string()),
        ),
        (
            "mcpServerKeyHash".to_string(),
            telemetry::otel::AttrValue::from(payload.mcp_server_key_hash.as_str().to_string()),
        ),
    ]);
    if let Some(error_code) = payload.error_code.as_ref() {
        attrs.insert(
            "errorCode".to_string(),
            telemetry::otel::AttrValue::from(error_code.as_str().to_string()),
        );
    }
    if let Some(base_url) = payload.mcp_server_base_url.as_ref() {
        attrs.insert(
            "mcpServerBaseUrl".to_string(),
            telemetry::otel::AttrValue::from(base_url.as_str().to_string()),
        );
    }
    telemetry::otel::emit_named_log_event(telemetry::tengu::mcp::SESSION_EXPIRED, &attrs);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::SESSION_EXPIRED,
        serde_json::to_value(payload).expect("serialize test session_expired payload"),
    );
}

pub(super) fn emit_server_needs_auth_for_config(config: &McpServerConfig, cause: Option<&str>) {
    let telemetry = oauth::McpOAuthTelemetryContext::for_server(&config.name, &config.spec);
    let payload = telemetry::tengu::mcp::ServerNeedsAuthPayload {
        transport_type: telemetry.transport_type,
        mcp_server_key_hash: telemetry.mcp_server_key_hash,
        cause: cause.map(|value| telemetry::Verified::assert_safe(value.to_string())),
    };
    telemetry::emit_mcp_server_needs_auth(&payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::SERVER_NEEDS_AUTH,
        serde_json::to_value(payload).unwrap(),
    );
}

pub(super) fn emit_tool_call_auth_error_for_config(
    config: &McpServerConfig,
    error_code: &str,
    auth_error_kind: telemetry::tengu::mcp::ToolCallAuthErrorKind,
) {
    let telemetry = oauth::McpOAuthTelemetryContext::for_server(&config.name, &config.spec);
    let payload = telemetry::tengu::mcp::ToolCallAuthErrorPayload {
        error_code: telemetry::Verified::assert_safe(error_code.to_string()),
        transport_type: telemetry.transport_type,
        auth_error_kind,
        mcp_server_key_hash: telemetry.mcp_server_key_hash,
    };
    telemetry::emit_mcp_tool_call_auth_error(&payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::TOOL_CALL_AUTH_ERROR,
        serde_json::to_value(payload).unwrap(),
    );
}

pub(super) fn oauth_refresh_failure_reason(error: &oauth::OAuthError) -> &'static str {
    match error {
        oauth::OAuthError::Discovery(_) => "metadata_discovery_failed",
        oauth::OAuthError::RefreshRejected(_) => "invalid_grant",
        oauth::OAuthError::Token(message) if message.contains("invalid_client") => "invalid_client",
        oauth::OAuthError::Token(message) if message.contains("unauthorized_client") => {
            "unauthorized_client"
        }
        oauth::OAuthError::Token(message) if message.contains("decode") => {
            "token_response_schema_rejected"
        }
        oauth::OAuthError::Token(_) => "request_failed",
        oauth::OAuthError::Callback(_) => "request_failed",
        oauth::OAuthError::Registration(_) => "request_failed",
    }
}

pub(super) fn tool_call_auth_error_code(error: &crate::client::McpClientError) -> &'static str {
    match error {
        crate::client::McpClientError::HttpResponse { status, .. } if *status == 403 => "403",
        crate::client::McpClientError::HttpResponse { .. } => "401",
        _ => "401",
    }
}

pub(super) fn xaa_flow_failure_stage(error: &crate::xaa::XaaError) -> &'static str {
    match error {
        crate::xaa::XaaError::TokenExchange { .. } => "token_exchange",
        crate::xaa::XaaError::JwtBearer(_) => "jwt_bearer",
        crate::xaa::XaaError::Prm(_)
        | crate::xaa::XaaError::NoAuthServer(_)
        | crate::xaa::XaaError::AsMetadata(_) => "discovery",
    }
}

pub(super) fn xaa_provider_failure_stage(error: &McpError) -> &'static str {
    match error {
        McpError::OAuth(message)
            if message.string_at("").is_some_and(|message| {
                message.starts_with("XAA IdP: OIDC discovery")
                    || message.starts_with("XAA IdP: refusing non-HTTPS token endpoint")
            }) =>
        {
            "discovery"
        }
        _ => "idp_login",
    }
}

pub(super) fn emit_server_connection_succeeded(
    payload: &telemetry::tengu::mcp::ServerConnectionSucceededPayload,
) {
    telemetry::emit_mcp_server_connection_succeeded(payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::SERVER_CONNECTION_SUCCEEDED,
        serde_json::to_value(payload).expect("serialize test success payload"),
    );
}

pub(super) fn emit_server_connection_failed(
    payload: &telemetry::tengu::mcp::ServerConnectionFailedPayload,
) {
    telemetry::emit_mcp_server_connection_failed(payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::SERVER_CONNECTION_FAILED,
        serde_json::to_value(payload).expect("serialize test failure payload"),
    );
}

pub(super) fn emit_tools_listed(payload: &telemetry::tengu::mcp::ToolsListedPayload) {
    telemetry::emit_mcp_tools_listed(payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::TOOLS_LISTED,
        serde_json::to_value(payload).expect("serialize test tools_listed payload"),
    );
}

pub(super) fn emit_degraded(payload: &telemetry::tengu::mcp::DegradedPayload) {
    telemetry::emit_mcp_degraded(payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::DEGRADED,
        serde_json::to_value(payload).expect("serialize test degraded payload"),
    );
}

pub(super) fn emit_list_changed(payload: &telemetry::tengu::mcp::ListChangedPayload) {
    telemetry::emit_mcp_list_changed(payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::LIST_CHANGED,
        serde_json::to_value(payload).expect("serialize test list_changed payload"),
    );
}

pub(super) fn emit_listen_reopen(payload: &telemetry::tengu::mcp::ListenReopenPayload) {
    telemetry::emit_mcp_listen_reopen(payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::LISTEN_REOPEN,
        serde_json::to_value(payload).expect("serialize test listen_reopen payload"),
    );
}

pub(super) fn emit_resource_templates_fetched(
    payload: &telemetry::tengu::mcp::ResourceTemplatesFetchedPayload,
) {
    telemetry::emit_mcp_resource_templates_fetched(payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::RESOURCE_TEMPLATES_FETCHED,
        serde_json::to_value(payload).expect("serialize test resource_templates payload"),
    );
}

pub(super) fn listen_reopen_payload(
    server_name: &str,
    outcome: telemetry::tengu::mcp::ListenReopenOutcome,
    attempts: u32,
    trigger: telemetry::tengu::mcp::ListenReopenTrigger,
) -> telemetry::tengu::mcp::ListenReopenPayload {
    telemetry::tengu::mcp::ListenReopenPayload {
        mcp_server_key_hash: mcp_server_key_hash(server_name),
        outcome,
        attempts,
        trigger,
    }
}

fn method_is(method: &[u16], expected: &str) -> bool {
    method.iter().copied().eq(expected.encode_utf16())
}

pub(super) fn notification_kind(method: &[u16]) -> Option<McpCatalogKind> {
    if method_is(method, "notifications/tools/list_changed") {
        Some(McpCatalogKind::Tools)
    } else if method_is(method, "notifications/prompts/list_changed") {
        Some(McpCatalogKind::Prompts)
    } else if method_is(method, "notifications/resources/list_changed") {
        Some(McpCatalogKind::Resources)
    } else {
        None
    }
}

pub(super) fn forward_catalog_change(
    changes: &broadcast::Sender<McpCatalogChanged>,
    server_name: &str,
    connection_id: McpConnectionId,
    method: &[u16],
    telemetry_cause: Option<&'static str>,
) {
    let Some(kind) = notification_kind(method) else {
        return;
    };
    let _ = changes.send(McpCatalogChanged {
        server_name: server_name.to_string(),
        connection_id,
        retired_connection_id: None,
        kind,
        telemetry_cause,
    });
}

/// §20b — build the `tengu_mcp_degraded` payload for every NONZERO bucket in
/// one server's tallied tool-schema classification counts. Pure and
/// deterministic (no telemetry emission, no tracing) so the aggregation
/// logic — which count-field a reason maps to, and that every nonzero
/// bucket becomes exactly one payload — is unit-testable directly, without
/// racing a concurrently-running test's `tracing` subscriber over the
/// process-global callsite `Interest` cache (see the call site's doc for
/// why that race is real, not hypothetical).
pub(super) fn degraded_payloads_for_server(
    counts: &std::collections::HashMap<telemetry::tengu::mcp::DegradedReason, u32>,
    transport_kind: &str,
    server_name: &str,
) -> Vec<telemetry::tengu::mcp::DegradedPayload> {
    use telemetry::pii::Verified;
    use telemetry::tengu::mcp::{DegradedPayload, DegradedReason};

    if counts.is_empty() {
        return Vec::new();
    }
    let transport_type = Verified::assert_safe(transport_kind.to_string());
    // Same gate as `tools_listed_payload` — the oracle spreads the SAME `P`
    // into every per-server `tengu_mcp_degraded`.
    let mcp_server_name = telemetry::tengu::mcp::server_name_gate(transport_kind)
        .then(|| Verified::assert_safe(server_name.to_string()));
    let mut out = Vec::with_capacity(counts.len());
    for (reason, count) in counts {
        let (normalized_count, skipped_count, kept_count) = match reason {
            // Oracle's `connected_zero_tools` payload is
            // `{reason,transportType,mcpServerName,..._}` — no count field.
            DegradedReason::ConnectedZeroTools
            | DegradedReason::ToolsListFailed
            | DegradedReason::ResourcesListFailed
            | DegradedReason::PromptsListFailed => (None, None, None),
            DegradedReason::ToolSchemaNormalized => (Some(*count), None, None),
            DegradedReason::ToolSchemaNormalizeGated
            | DegradedReason::ToolSchemaUnsupported
            | DegradedReason::ToolSchemaInvalid
            | DegradedReason::ToolPropertyKeyInvalid => (None, Some(*count), None),
            DegradedReason::ToolSchemaInvalidGated
            | DegradedReason::ToolPropertyKeyInvalidGated => (None, None, Some(*count)),
            // `SchemaValidatorUnavailable` is process-global (fired from
            // `tool_schema::meta_validator`, never tallied into this
            // per-server map) and the enum is `#[non_exhaustive]` — a future
            // oracle-confirmed sibling with no known count-field mapping
            // falls here too, skipped rather than guessed at.
            _ => continue,
        };
        out.push(DegradedPayload {
            reason: *reason,
            transport_type: Some(transport_type.clone()),
            normalized_count,
            skipped_count,
            kept_count,
            mcp_server_name: mcp_server_name.clone(),
        });
    }
    out
}
