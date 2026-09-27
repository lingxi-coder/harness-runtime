//! First-party cloud-provider environment surface (Vertex / Foundry / Bedrock).
//!
//! Claude Code selects one of four API providers via `getAPIProvider()`
//! (`utils/model/providers.ts`): `bedrock` / `vertex` / `foundry` / `firstParty`,
//! gated on `CLAUDE_CODE_USE_{BEDROCK,VERTEX,FOUNDRY}` env truthiness. For the
//! three managed-cloud providers it then derives the request base URL, the
//! per-model region, and the auth-skip gate from a family of `ANTHROPIC_*` /
//! `CLAUDE_CODE_SKIP_*` / `CLOUD_ML_REGION` / `VERTEX_REGION_CLAUDE_*` env vars.
//!
//! This module ports that **env → URL / region / auth-gate** surface
//! byte-faithfully so the workspace can build a consistent managed-cloud route
//! instead of the half-ported state where `CLAUDE_CODE_USE_VERTEX=1` flips the
//! default model family (see `agent::model_resolution`) while requests still
//! route to the active (e.g. first-party) transport.
//!
//! ## Ported byte-exactly (CC 2.1.207)
//!
//! - Vertex default region — `ATn()`:
//!   `return process.env.CLOUD_ML_REGION||"us-east5"`.
//! - Vertex base host — `hBt(region)`:
//!   ```js
//!   switch(region){
//!     case"global": return"https://aiplatform.googleapis.com";
//!     case"us": case"eu": return`https://aiplatform.${region}.rep.googleapis.com`;
//!     default: return`https://${region}-aiplatform.googleapis.com`;
//!   }
//!   ```
//! - Per-model Vertex region — `dke(model)` over the `l4f()` table:
//!   ```js
//!   let t = l4f().find(([prefix]) => model.startsWith(prefix));
//!   return t ? (process.env[t.envVar] || ATn()) : ATn();
//!   ```
//!   where `l4f()` is the catalog `[[wTn(id), vertex_region_env_var], ...]`
//!   pairs (`wTn(e)=e.replace(/-0$/,"")`) sorted by prefix length **descending**
//!   then prefix lexical **ascending**, so `find` matches the most-specific
//!   prefix.
//! - Vertex request base host — the `getBaseURL()` `case"vertex"` arm:
//!   `process.env.ANTHROPIC_VERTEX_BASE_URL || hBt(dke(model))`.
//! - Foundry base host — `M_i()`:
//!   ```js
//!   process.env.ANTHROPIC_FOUNDRY_BASE_URL ||
//!     (process.env.ANTHROPIC_FOUNDRY_RESOURCE
//!        ? `https://${ANTHROPIC_FOUNDRY_RESOURCE}.services.ai.azure.com` : undefined)
//!   ```
//!   The Foundry Anthropic-messages endpoint appends `/anthropic/` when derived
//!   from `RESOURCE` (`https://{resource}.services.ai.azure.com/anthropic/`).
//! - Bedrock base-url override (`ANTHROPIC_BEDROCK_BASE_URL`),
//!   `ANTHROPIC_SMALL_FAST_MODEL_AWS_REGION`, and the
//!   `CLAUDE_CODE_SKIP_{VERTEX,BEDROCK,FOUNDRY}_AUTH` gates.
//!
//! ## Documented remainder (NOT in this module)
//!
//! - **GCP ADC auth** (`google-auth-library` / `GoogleAuth` /
//!   `application_default_credentials`): minting/refreshing the Vertex bearer
//!   token. [`VertexClaudeCodec`](crate::VertexClaudeCodec) already accepts a
//!   static bearer via [`AuthStrategy::GcpToken`](crate::AuthStrategy); wiring an
//!   ADC token source that feeds it is future work.
//! - **Boot-time route synthesis**: assembling a [`ProviderProfile`] from these
//!   values at client build time (so `CLAUDE_CODE_USE_VERTEX=1` /
//!   `CLAUDE_CODE_USE_FOUNDRY=1` actually route through the managed-cloud codec)
//!   and the `/setup-vertex` /`/setup-bedrock` reconfigure commands.
//!
//! ## Landed since (parity 2.1.207 H-BIN-10)
//!
//! - **Foundry transport codec**: [`FoundryClaudeCodec`](crate::FoundryClaudeCodec)
//!   speaks the Anthropic Messages wire at
//!   [`foundry_messages_base_url`]'s `.../anthropic/` root, reachable via a
//!   `foundry-claude` [`ProviderProfile`] ([`ProtocolFamily::FoundryClaude`]);
//!   [`FoundryCredential::ApiKey`] → `x-api-key`, [`FoundryCredential::AuthToken`]
//!   (AAD) → `Authorization: Bearer`. (`azure_openai.rs` remains the `OpenAI`
//!   protocol, unrelated to the Foundry Messages API.)
//!
//! [`ProtocolFamily::FoundryClaude`]: crate::ProtocolFamily::FoundryClaude
//!
//! [`ProviderProfile`]: crate::ProviderProfile

/// Vertex AI default region — `ATn()`
/// (`return process.env.CLOUD_ML_REGION||"us-east5"`).
///
/// Pure: pass the value of `CLOUD_ML_REGION` (or `None` if unset/empty).
#[must_use]
pub fn vertex_default_region(cloud_ml_region: Option<&str>) -> String {
    match cloud_ml_region {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => "us-east5".to_string(),
    }
}

/// Vertex AI base host for a region — `hBt(region)`.
///
/// - `global` → `https://aiplatform.googleapis.com`
/// - `us` / `eu` → `https://aiplatform.{region}.rep.googleapis.com`
/// - otherwise → `https://{region}-aiplatform.googleapis.com`
#[must_use]
pub fn vertex_base_host(region: &str) -> String {
    match region {
        "global" => "https://aiplatform.googleapis.com".to_string(),
        "us" | "eu" => format!("https://aiplatform.{region}.rep.googleapis.com"),
        _ => format!("https://{region}-aiplatform.googleapis.com"),
    }
}

/// Catalog `[model-id-prefix, VERTEX_REGION_CLAUDE_* env var]` pairs — the
/// `l4f()` table, pre-sorted by prefix length **descending** then prefix lexical
/// **ascending** so [`vertex_region_env_var_for_model`] returns the
/// most-specific match first (mirrors `l4f().find(...)`).
///
/// Prefixes are `wTn(id)` = `id.replace(/-0$/,"")` over the CC 2.1.207 model
/// catalog `vertex_region_env_var` entries (note the `-0` strip: `claude-opus-4-0`
/// → `claude-opus-4`, `claude-sonnet-4-0` → `claude-sonnet-4`).
///
/// The ordering invariant is asserted by `region_table_is_sorted_len_desc_lex_asc`.
const VERTEX_REGION_TABLE: &[(&str, &str)] = &[
    ("claude-3-5-sonnet", "VERTEX_REGION_CLAUDE_3_5_SONNET"),
    ("claude-3-7-sonnet", "VERTEX_REGION_CLAUDE_3_7_SONNET"),
    ("claude-sonnet-4-5", "VERTEX_REGION_CLAUDE_4_5_SONNET"),
    ("claude-sonnet-4-6", "VERTEX_REGION_CLAUDE_4_6_SONNET"),
    ("claude-3-5-haiku", "VERTEX_REGION_CLAUDE_3_5_HAIKU"),
    ("claude-fable-5-1", "VERTEX_REGION_CLAUDE_FABLE_5"),
    ("claude-haiku-4-5", "VERTEX_REGION_CLAUDE_HAIKU_4_5"),
    ("claude-opus-4-1", "VERTEX_REGION_CLAUDE_4_1_OPUS"),
    ("claude-opus-4-5", "VERTEX_REGION_CLAUDE_4_5_OPUS"),
    ("claude-opus-4-6", "VERTEX_REGION_CLAUDE_4_6_OPUS"),
    ("claude-opus-4-7", "VERTEX_REGION_CLAUDE_4_7_OPUS"),
    ("claude-opus-4-8", "VERTEX_REGION_CLAUDE_4_8_OPUS"),
    ("claude-sonnet-4", "VERTEX_REGION_CLAUDE_4_0_SONNET"),
    ("claude-sonnet-5", "VERTEX_REGION_CLAUDE_5_SONNET"),
    ("claude-opus-4", "VERTEX_REGION_CLAUDE_4_0_OPUS"),
    // Note the ORDER of the name parts: the oracle spells this
    // `VERTEX_REGION_CLAUDE_5_OPUS`, not `..._OPUS_5` and not `..._5_0_OPUS`.
    // Placed here to keep the len-desc/lex-asc invariant the prefix match
    // depends on (13 chars, after `claude-opus-4`).
    ("claude-opus-5", "VERTEX_REGION_CLAUDE_5_OPUS"),
];

/// The `VERTEX_REGION_CLAUDE_*` env var name whose region pin applies to `model`,
/// or `None` if no catalog prefix matches — the prefix lookup inside `dke()`
/// (`l4f().find(([prefix]) => model.startsWith(prefix))`).
#[must_use]
pub fn vertex_region_env_var_for_model(model: &str) -> Option<&'static str> {
    VERTEX_REGION_TABLE
        .iter()
        .find(|(prefix, _)| model.starts_with(prefix))
        .map(|(_, env_var)| *env_var)
}

/// Per-model Vertex region — `dke(model)`.
///
/// Pure: `region_pin` is the value of the model's `VERTEX_REGION_CLAUDE_*` env
/// var (or `None` if that var is unset/empty *or* no catalog prefix matched);
/// `cloud_ml_region` is the value of `CLOUD_ML_REGION`. When the pin is present
/// it wins; otherwise falls back to [`vertex_default_region`].
#[must_use]
pub fn vertex_region_for_model(region_pin: Option<&str>, cloud_ml_region: Option<&str>) -> String {
    match region_pin {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => vertex_default_region(cloud_ml_region),
    }
}

/// Vertex request base host — `getBaseURL()`'s `case"vertex"`:
/// `process.env.ANTHROPIC_VERTEX_BASE_URL || hBt(region)`.
///
/// Pure: `base_url_override` is `ANTHROPIC_VERTEX_BASE_URL`; `region` is the
/// already-resolved [`vertex_region_for_model`] value.
#[must_use]
pub fn vertex_base_host_url(base_url_override: Option<&str>, region: &str) -> String {
    match base_url_override {
        Some(u) if !u.is_empty() => u.to_string(),
        _ => vertex_base_host(region),
    }
}

/// Full Vertex prefix that [`VertexClaudeCodec`](crate::VertexClaudeCodec)
/// expects as its `base_url`: the request host plus the
/// `/v1/projects/{project}/locations/{region}` path segment the Vertex SDK
/// always appends (the codec then appends `/publishers/anthropic/models/…`).
///
/// `host` is [`vertex_base_host_url`]; `project` is `ANTHROPIC_VERTEX_PROJECT_ID`;
/// `region` is the resolved region (also used for the path `locations/` segment).
#[must_use]
pub fn vertex_codec_base_url(host: &str, project: &str, region: &str) -> String {
    let host = host.trim_end_matches('/');
    format!("{host}/v1/projects/{project}/locations/{region}")
}

/// Foundry base host — `M_i()`:
/// `ANTHROPIC_FOUNDRY_BASE_URL ||
///  (ANTHROPIC_FOUNDRY_RESOURCE ? "https://{resource}.services.ai.azure.com" : undefined)`.
///
/// Used for provider identity (`N_i` builds `"{host}::{model}"`). `None` when
/// neither var is set.
#[must_use]
pub fn foundry_base_host(base_url: Option<&str>, resource: Option<&str>) -> Option<String> {
    match base_url {
        Some(u) if !u.is_empty() => Some(u.to_string()),
        _ => match resource {
            Some(r) if !r.is_empty() => Some(format!("https://{r}.services.ai.azure.com")),
            _ => None,
        },
    }
}

/// Foundry Anthropic-messages request base URL — the Foundry SDK constructor:
/// `ANTHROPIC_FOUNDRY_BASE_URL` used verbatim when set, else derived from
/// `ANTHROPIC_FOUNDRY_RESOURCE` as `https://{resource}.services.ai.azure.com/anthropic/`.
///
/// `base_url` and `resource` are mutually exclusive in CC (the constructor
/// throws when both are set); this helper honors `base_url` first to match
/// `M_i()`'s precedence and returns `None` when neither is set.
#[must_use]
pub fn foundry_messages_base_url(base_url: Option<&str>, resource: Option<&str>) -> Option<String> {
    match base_url {
        Some(u) if !u.is_empty() => Some(u.to_string()),
        _ => match resource {
            Some(r) if !r.is_empty() => {
                Some(format!("https://{r}.services.ai.azure.com/anthropic/"))
            }
            _ => None,
        },
    }
}

/// Foundry credential selection from `ANTHROPIC_FOUNDRY_API_KEY` /
/// `ANTHROPIC_FOUNDRY_AUTH_TOKEN`. The API key takes precedence when both are
/// present (mirrors the Foundry SDK `apiKey ?? authToken` precedence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoundryCredential {
    /// `ANTHROPIC_FOUNDRY_API_KEY` — sent as the `x-api-key` header (CC 2.1.207
    /// `AnthropicFoundry.authHeaders()`: a string `apiKey` ⇒
    /// `{"x-api-key": apiKey}`, the same header first-party Anthropic uses).
    ApiKey(String),
    /// `ANTHROPIC_FOUNDRY_AUTH_TOKEN` (AAD `Authorization: Bearer` token — the
    /// SDK's `azureADTokenProvider` function path).
    AuthToken(String),
    /// Neither set — auth deferred (see [`skip_foundry_auth`]).
    None,
}

/// Select the Foundry credential from the two env values (pure).
#[must_use]
pub fn select_foundry_credential(
    api_key: Option<&str>,
    auth_token: Option<&str>,
) -> FoundryCredential {
    if let Some(k) = api_key.filter(|s| !s.is_empty()) {
        return FoundryCredential::ApiKey(k.to_string());
    }
    if let Some(t) = auth_token.filter(|s| !s.is_empty()) {
        return FoundryCredential::AuthToken(t.to_string());
    }
    FoundryCredential::None
}

// ── Env-reading convenience wrappers ─────────────────────────────────────────
//
// Thin adapters that read `std::env` and delegate to the pure cores above. The
// byte-faithful logic is unit-tested through the pure functions (env-var writes
// are process-global and racy under the parallel test harness).

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// `dke(model)` reading the process environment: resolve the per-model Vertex
/// region from `VERTEX_REGION_CLAUDE_*` / `CLOUD_ML_REGION`.
#[must_use]
pub fn vertex_region_for_model_from_env(model: &str) -> String {
    let pin = vertex_region_env_var_for_model(model).and_then(env_nonempty);
    let cloud_ml_region = env_nonempty("CLOUD_ML_REGION");
    vertex_region_for_model(pin.as_deref(), cloud_ml_region.as_deref())
}

/// Full [`VertexClaudeCodec`](crate::VertexClaudeCodec) base URL for `model`
/// read from the process environment, or `None` when `ANTHROPIC_VERTEX_PROJECT_ID`
/// is not set (CC requires it to build the Vertex request path).
#[must_use]
pub fn vertex_codec_base_url_from_env(model: &str) -> Option<String> {
    let project = env_nonempty("ANTHROPIC_VERTEX_PROJECT_ID")?;
    let region = vertex_region_for_model_from_env(model);
    let host = vertex_base_host_url(
        env_nonempty("ANTHROPIC_VERTEX_BASE_URL").as_deref(),
        &region,
    );
    Some(vertex_codec_base_url(&host, &project, &region))
}

/// `CLAUDE_CODE_SKIP_VERTEX_AUTH` truthiness.
#[must_use]
pub fn skip_vertex_auth() -> bool {
    platform_api::env::is_env_truthy(
        std::env::var("CLAUDE_CODE_SKIP_VERTEX_AUTH")
            .ok()
            .as_deref(),
    )
}

/// `CLAUDE_CODE_SKIP_FOUNDRY_AUTH` truthiness.
#[must_use]
pub fn skip_foundry_auth() -> bool {
    platform_api::env::is_env_truthy(
        std::env::var("CLAUDE_CODE_SKIP_FOUNDRY_AUTH")
            .ok()
            .as_deref(),
    )
}

/// `CLAUDE_CODE_SKIP_BEDROCK_AUTH` truthiness.
#[must_use]
pub fn skip_bedrock_auth() -> bool {
    platform_api::env::is_env_truthy(
        std::env::var("CLAUDE_CODE_SKIP_BEDROCK_AUTH")
            .ok()
            .as_deref(),
    )
}

/// `ANTHROPIC_BEDROCK_BASE_URL` override (`getBaseURL()`'s `case"bedrock"` head),
/// or `None` when unset/empty.
#[must_use]
pub fn bedrock_base_url_override() -> Option<String> {
    env_nonempty("ANTHROPIC_BEDROCK_BASE_URL")
}

/// `ANTHROPIC_SMALL_FAST_MODEL_AWS_REGION` — the separate AWS region for the
/// small/fast (background) model on Bedrock (`T$n`), or `None` when unset/empty.
#[must_use]
pub fn small_fast_model_aws_region() -> Option<String> {
    env_nonempty("ANTHROPIC_SMALL_FAST_MODEL_AWS_REGION")
}

/// Foundry base host read from the process environment (`M_i()`).
#[must_use]
pub fn foundry_base_host_from_env() -> Option<String> {
    foundry_base_host(
        env_nonempty("ANTHROPIC_FOUNDRY_BASE_URL").as_deref(),
        env_nonempty("ANTHROPIC_FOUNDRY_RESOURCE").as_deref(),
    )
}

/// Foundry messages base URL read from the process environment.
#[must_use]
pub fn foundry_messages_base_url_from_env() -> Option<String> {
    foundry_messages_base_url(
        env_nonempty("ANTHROPIC_FOUNDRY_BASE_URL").as_deref(),
        env_nonempty("ANTHROPIC_FOUNDRY_RESOURCE").as_deref(),
    )
}

/// Foundry credential read from the process environment.
#[must_use]
pub fn foundry_credential_from_env() -> FoundryCredential {
    select_foundry_credential(
        env_nonempty("ANTHROPIC_FOUNDRY_API_KEY").as_deref(),
        env_nonempty("ANTHROPIC_FOUNDRY_AUTH_TOKEN").as_deref(),
    )
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Vertex default region — ATn() ──────────────────────────────────────

    #[test]
    fn vertex_default_region_matches_atn() {
        // CLOUD_ML_REGION wins when non-empty.
        assert_eq!(vertex_default_region(Some("europe-west1")), "europe-west1");
        // Unset / empty → the CC default "us-east5".
        assert_eq!(vertex_default_region(None), "us-east5");
        assert_eq!(vertex_default_region(Some("")), "us-east5");
    }

    // ── Vertex base host — hBt() ────────────────────────────────────────────

    #[test]
    fn vertex_base_host_matches_hbt_switch() {
        assert_eq!(
            vertex_base_host("global"),
            "https://aiplatform.googleapis.com"
        );
        assert_eq!(
            vertex_base_host("us"),
            "https://aiplatform.us.rep.googleapis.com"
        );
        assert_eq!(
            vertex_base_host("eu"),
            "https://aiplatform.eu.rep.googleapis.com"
        );
        assert_eq!(
            vertex_base_host("us-east5"),
            "https://us-east5-aiplatform.googleapis.com"
        );
        assert_eq!(
            vertex_base_host("europe-west1"),
            "https://europe-west1-aiplatform.googleapis.com"
        );
    }

    // ── Region table — l4f() ordering + wTn(-0 strip) ───────────────────────

    /// The table must be sorted by prefix length descending, then prefix lexical
    /// ascending, so `find` (linear) resolves the most-specific `startsWith`
    /// match first — exactly `l4f().sort(([a],[b]) => b.length-a.length || a<b?-1:...)`.
    /// Opus 5 was absent from this port entirely — no catalog entry, no region
    /// pin, no 1M context — despite being the oracle's 2.1.219 flagship.
    #[test]
    fn opus_5_has_its_vertex_region_pin() {
        assert_eq!(
            vertex_region_env_var_for_model("claude-opus-5"),
            Some("VERTEX_REGION_CLAUDE_5_OPUS")
        );
        // Must not be captured by the shorter `claude-opus-4` prefix.
        assert_eq!(
            vertex_region_env_var_for_model("claude-opus-4"),
            Some("VERTEX_REGION_CLAUDE_4_0_OPUS")
        );
    }

    #[test]
    fn region_table_is_sorted_len_desc_lex_asc() {
        for pair in VERTEX_REGION_TABLE.windows(2) {
            let (a, _) = pair[0];
            let (b, _) = pair[1];
            let ok = a.len() > b.len() || (a.len() == b.len() && a < b);
            assert!(
                ok,
                "table ordering violated at {a:?} -> {b:?} (need len desc then lexical asc)"
            );
        }
    }

    /// `wTn(id)=id.replace(/-0$/,"")` — the `-0` families use the stripped prefix.
    #[test]
    fn region_table_reflects_wtn_minus_zero_strip() {
        // claude-opus-4-0 and claude-sonnet-4-0 collapse to the -0-stripped prefix.
        assert_eq!(
            vertex_region_env_var_for_model("claude-opus-4-20250514"),
            Some("VERTEX_REGION_CLAUDE_4_0_OPUS")
        );
        assert_eq!(
            vertex_region_env_var_for_model("claude-sonnet-4-20250514"),
            Some("VERTEX_REGION_CLAUDE_4_0_SONNET")
        );
    }

    /// `dke`'s prefix `find`: most-specific prefix wins; unknown models get None.
    #[test]
    fn region_env_var_for_model_prefix_matching() {
        // Specific 4-5 / 4-6 win over the -0-stripped claude-sonnet-4.
        assert_eq!(
            vertex_region_env_var_for_model("claude-sonnet-4-5-20250929"),
            Some("VERTEX_REGION_CLAUDE_4_5_SONNET")
        );
        assert_eq!(
            vertex_region_env_var_for_model("claude-sonnet-4-6"),
            Some("VERTEX_REGION_CLAUDE_4_6_SONNET")
        );
        // Vertex `@`-style id still starts with the stripped family prefix.
        assert_eq!(
            vertex_region_env_var_for_model("claude-sonnet-4@20250514"),
            Some("VERTEX_REGION_CLAUDE_4_0_SONNET")
        );
        assert_eq!(
            vertex_region_env_var_for_model("claude-opus-4-8"),
            Some("VERTEX_REGION_CLAUDE_4_8_OPUS")
        );
        assert_eq!(
            vertex_region_env_var_for_model("claude-fable-5-1"),
            Some("VERTEX_REGION_CLAUDE_FABLE_5")
        );
        assert_eq!(
            vertex_region_env_var_for_model("claude-sonnet-5"),
            Some("VERTEX_REGION_CLAUDE_5_SONNET")
        );
        // No catalog prefix → None (dke falls back to ATn()).
        assert_eq!(vertex_region_env_var_for_model("gpt-4o"), None);
        assert_eq!(vertex_region_env_var_for_model("claude-2.1"), None);
    }

    // ── Per-model region — dke() ────────────────────────────────────────────

    #[test]
    fn vertex_region_for_model_precedence() {
        // Pin beats CLOUD_ML_REGION beats default.
        assert_eq!(
            vertex_region_for_model(Some("asia-southeast1"), Some("europe-west1")),
            "asia-southeast1"
        );
        assert_eq!(
            vertex_region_for_model(None, Some("europe-west1")),
            "europe-west1"
        );
        assert_eq!(vertex_region_for_model(None, None), "us-east5");
        // Empty pin is treated as unset.
        assert_eq!(
            vertex_region_for_model(Some(""), Some("europe-west1")),
            "europe-west1"
        );
    }

    // ── Vertex base host url — case"vertex" ─────────────────────────────────

    #[test]
    fn vertex_base_host_url_prefers_explicit_override() {
        assert_eq!(
            vertex_base_host_url(Some("https://my-proxy.example.com"), "us-east5"),
            "https://my-proxy.example.com"
        );
        assert_eq!(
            vertex_base_host_url(None, "us-east5"),
            "https://us-east5-aiplatform.googleapis.com"
        );
        assert_eq!(
            vertex_base_host_url(Some(""), "global"),
            "https://aiplatform.googleapis.com"
        );
    }

    // ── Codec base URL (full prefix for VertexClaudeCodec) ───────────────────

    #[test]
    fn vertex_codec_base_url_appends_project_location_path() {
        assert_eq!(
            vertex_codec_base_url(
                "https://us-east5-aiplatform.googleapis.com",
                "my-proj",
                "us-east5"
            ),
            "https://us-east5-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-east5"
        );
        // Trailing slash on host is normalized.
        assert_eq!(
            vertex_codec_base_url("https://aiplatform.googleapis.com/", "p", "global"),
            "https://aiplatform.googleapis.com/v1/projects/p/locations/global"
        );
    }

    /// End-to-end: the codec base URL a `claude-opus-4-6` request would route to,
    /// composed from region + host + project — the value that closes the
    /// half-ported state (models flipped to the Vertex family AND transport
    /// pointed at Vertex).
    #[test]
    fn vertex_codec_base_url_end_to_end_composition() {
        // Pin present for 4-6-opus.
        let region = vertex_region_for_model(Some("us-east5"), None);
        let host = vertex_base_host_url(None, &region);
        assert_eq!(
            vertex_codec_base_url(&host, "acme-vertex", &region),
            "https://us-east5-aiplatform.googleapis.com/v1/projects/acme-vertex/locations/us-east5"
        );
    }

    // ── Foundry — M_i() ─────────────────────────────────────────────────────

    #[test]
    fn foundry_base_host_matches_m_i() {
        // BASE_URL wins.
        assert_eq!(
            foundry_base_host(Some("https://custom.example.com"), Some("res")),
            Some("https://custom.example.com".to_string())
        );
        // RESOURCE-derived host (no /anthropic/ on the identity host).
        assert_eq!(
            foundry_base_host(None, Some("my-res")),
            Some("https://my-res.services.ai.azure.com".to_string())
        );
        // Neither → None.
        assert_eq!(foundry_base_host(None, None), None);
        assert_eq!(foundry_base_host(Some(""), Some("")), None);
    }

    #[test]
    fn foundry_messages_base_url_appends_anthropic_path_for_resource() {
        assert_eq!(
            foundry_messages_base_url(None, Some("my-res")),
            Some("https://my-res.services.ai.azure.com/anthropic/".to_string())
        );
        // Explicit base URL used verbatim (assumed to already carry the path).
        assert_eq!(
            foundry_messages_base_url(Some("https://custom/anthropic/"), None),
            Some("https://custom/anthropic/".to_string())
        );
        assert_eq!(foundry_messages_base_url(None, None), None);
    }

    #[test]
    fn foundry_credential_precedence() {
        assert_eq!(
            select_foundry_credential(Some("k"), Some("t")),
            FoundryCredential::ApiKey("k".to_string())
        );
        assert_eq!(
            select_foundry_credential(None, Some("t")),
            FoundryCredential::AuthToken("t".to_string())
        );
        assert_eq!(
            select_foundry_credential(Some(""), Some("t")),
            FoundryCredential::AuthToken("t".to_string())
        );
        assert_eq!(
            select_foundry_credential(None, None),
            FoundryCredential::None
        );
    }
}
