// lingxi-code/crates/core/src/settings/merger.rs
//! Per-field merge dispatcher.
//!
//! Signature is consume-and-return because every layer is short-lived during
//! [`crate::settings::Settings::load`]. Strategy choice routes through
//! [`crate::settings::schema::strategy_for`] — Task 5 wires up
//! [`MergeStrategy::ConcatDedup`]; Task 6 adds `DeepMerge` + `Override`.

use crate::settings::schema::{strategy_for, MergeStrategy, SettingsJson};

/// Merge two settings layers — `next` overlays `prev` per field strategy.
///
/// Locked signature: every caller (loader, tests, parity driver) uses this
/// exact form. Do not change without updating every call site.
/// Per-field merge for `settings.attribution` — lodash `merge` semantics for a
/// plain object: each field independently takes `next` when set, else `prev`.
///
/// NOT `next.or(prev)`: that would let a layer naming only `pr` erase an
/// inherited `commit`, which is the opposite of what a per-trailer override is
/// for. An explicit empty string is a VALUE and survives the merge.
fn merge_attribution(
    prev: Option<crate::settings::schema::Attribution>,
    next: Option<crate::settings::schema::Attribution>,
) -> Option<crate::settings::schema::Attribution> {
    match (prev, next) {
        (None, other) | (other, None) => other,
        (Some(prev), Some(next)) => Some(crate::settings::schema::Attribution {
            commit: next.commit.or(prev.commit),
            pr: next.pr.or(prev.pr),
        }),
    }
}

#[must_use]
pub fn merge(prev: SettingsJson, next: SettingsJson) -> SettingsJson {
    SettingsJson {
        teammate_mode: next.teammate_mode.or(prev.teammate_mode),
        dollar_schema: next.dollar_schema.or(prev.dollar_schema),
        trusted_directories: concat_dedup(prev.trusted_directories, next.trusted_directories),
        additional_directories: concat_dedup(
            prev.additional_directories,
            next.additional_directories,
        ),
        enabled_tools: concat_dedup(prev.enabled_tools, next.enabled_tools),
        additional_includes: concat_dedup(prev.additional_includes, next.additional_includes),
        lingxi_md_excludes: concat_dedup(prev.lingxi_md_excludes, next.lingxi_md_excludes),
        sandbox: deep_merge_object(prev.sandbox, next.sandbox),
        hooks: deep_merge_object(prev.hooks, next.hooks),
        // Deep-merge the opaque block. NOTE: claude-code concat-dedups the
        // allow/deny/ask arrays across tiers; deep_merge_object takes `next`
        // for a matching array key. This only affects a reader of the MERGED
        // field — the permission loader reads each settings FILE per-source
        // (`permission::permission_rules_from_settings_json`), so it is moot
        // for rule loading. (Refine to per-array concat if the merged field is
        // ever consumed directly.)
        permissions: deep_merge_object(prev.permissions, next.permissions),
        // Scalar fields — Override semantics: next wins when set, else prev.
        // OUTSTYLE.1: `outputStyle` is a string in claude-code and merges
        // scalar-override (settingsMergeCustomizer special-cases only arrays).
        output_style: next.output_style.or(prev.output_style),
        telemetry_enabled: next.telemetry_enabled.or(prev.telemetry_enabled),
        // Scalar Override (later source wins), same as `telemetryEnabled`.
        ax_screen_reader: next.ax_screen_reader.or(prev.ax_screen_reader),
        // Scalar Override (later source wins) — `alwaysThinkingEnabled`.
        always_thinking_enabled: next
            .always_thinking_enabled
            .or(prev.always_thinking_enabled),
        // Scalar Override (later source wins) — `skipWebFetchPreflight` (P2-14).
        skip_web_fetch_preflight: next
            .skip_web_fetch_preflight
            .or(prev.skip_web_fetch_preflight),
        // Scalar Override (later source wins) — `disableArtifact`/`enableArtifact`
        // (parity 2.1.207 H-BIN-03), same as `skipWebFetchPreflight`.
        disable_artifact: next.disable_artifact.or(prev.disable_artifact),
        enable_artifact: next.enable_artifact.or(prev.enable_artifact),
        // Scalar Override (later source wins) — `disableAgentView` (M-03), same
        // as `enableArtifact`.
        disable_agent_view: next.disable_agent_view.or(prev.disable_agent_view),
        disable_all_hooks: next.disable_all_hooks.or(prev.disable_all_hooks),
        allow_managed_hooks_only: next
            .allow_managed_hooks_only
            .or(prev.allow_managed_hooks_only),
        // Scalar Override — `askUserQuestionTimeout` (enum 60s|5m|10m|never).
        ask_user_question_timeout: next
            .ask_user_question_timeout
            .or(prev.ask_user_question_timeout),
        dialog_expiry: next.dialog_expiry.or(prev.dialog_expiry),
        cross_session_inbound: next.cross_session_inbound.or(prev.cross_session_inbound),
        policy_helpers: deep_merge_object(prev.policy_helpers, next.policy_helpers),
        sync_claude_ai_skills: next.sync_claude_ai_skills.or(prev.sync_claude_ai_skills),
        additional_marketplaces: deep_merge_object(
            prev.additional_marketplaces,
            next.additional_marketplaces,
        ),
        allowed_marketplaces: next.allowed_marketplaces.or(prev.allowed_marketplaces),
        disable_command_plugin_sources: next
            .disable_command_plugin_sources
            .or(prev.disable_command_plugin_sources),
        spellcheck: deep_merge_object(prev.spellcheck, next.spellcheck),
        model_proposed_goals: next.model_proposed_goals.or(prev.model_proposed_goals),
        keybinding_flavor: next.keybinding_flavor.or(prev.keybinding_flavor),
        auto_continue_at_usage_limit: next
            .auto_continue_at_usage_limit
            .or(prev.auto_continue_at_usage_limit),
        process_wrapper: next.process_wrapper.or(prev.process_wrapper),
        status_line: next.status_line.or(prev.status_line),
        subagent_status_line: next.subagent_status_line.or(prev.subagent_status_line),
        // Scalar Override — `viewMode` (enum default|verbose|focus), H-BIN-11.
        view_mode: next.view_mode.or(prev.view_mode),
        // Scalar Override — `emojiCompletionEnabled` (default true at use).
        emoji_completion_enabled: next
            .emoji_completion_enabled
            .or(prev.emoji_completion_enabled),
        // Scalar Override — `showThinkingSummaries` (default false at use).
        show_thinking_summaries: next
            .show_thinking_summaries
            .or(prev.show_thinking_summaries),
        vision_delegation_enabled: next
            .vision_delegation_enabled
            .or(prev.vision_delegation_enabled),
        agent_push_notif_enabled: next
            .agent_push_notif_enabled
            .or(prev.agent_push_notif_enabled),
        task_output_max_chars: next.task_output_max_chars.or(prev.task_output_max_chars),
        workflow_keyword_trigger_enabled: next
            .workflow_keyword_trigger_enabled
            .or(prev.workflow_keyword_trigger_enabled),
        auto_memory_enabled: next.auto_memory_enabled.or(prev.auto_memory_enabled),
        effort_level: next.effort_level.or(prev.effort_level),
        // Cap fold (lowest wins) is done at the /effort consumer over raw
        // files; the merged view is still later-source-wins like other scalars.
        max_effort_level: next.max_effort_level.or(prev.max_effort_level),
        model_settings: merge_model_settings(prev.model_settings, next.model_settings),
        bash_output_max_chars: next.bash_output_max_chars.or(prev.bash_output_max_chars),
        // `attribution` is a plain OBJECT, and the upstream customizer
        // special-cases only arrays (see the note above `permissions`), so two
        // layers deep-merge per field rather than the later one replacing the
        // whole object: a user-level `attribution.commit` survives a project
        // that sets only `attribution.pr`.
        attribution: merge_attribution(prev.attribution, next.attribution),
        include_co_authored_by: next.include_co_authored_by.or(prev.include_co_authored_by),
        include_git_instructions: next
            .include_git_instructions
            .or(prev.include_git_instructions),
        bash_edit_diff_enabled: next.bash_edit_diff_enabled.or(prev.bash_edit_diff_enabled),
        enable_workflows: next.enable_workflows.or(prev.enable_workflows),
        workflow_size_guideline: next
            .workflow_size_guideline
            .or(prev.workflow_size_guideline),
        model: next.model.or(prev.model),
        // Managed model-restriction keys (H-BIN-08). `availableModels` (array)
        // and `enforceAvailableModels` (scalar) are scalar-override — CC's
        // `settingsMergeCustomizer` returns the source array for non-concat
        // arrays. `modelOverrides` (record) deep-merges per key (next wins).
        available_models: next.available_models.or(prev.available_models),
        enforce_available_models: next
            .enforce_available_models
            .or(prev.enforce_available_models),
        model_overrides: merge_string_map(prev.model_overrides, next.model_overrides),
        // 2.1.198 AWS/GCP auth-refresh script keys — plain strings, scalar
        // Override (later source wins), same as `model`/`outputStyle`.
        aws_auth_refresh: next.aws_auth_refresh.or(prev.aws_auth_refresh),
        aws_credential_export: next.aws_credential_export.or(prev.aws_credential_export),
        gcp_auth_refresh: next.gcp_auth_refresh.or(prev.gcp_auth_refresh),
        // HTTP-hook security allowlists (H-BIN-12) — both array-merge
        // (concat-dedup): CC `settingsMergeCustomizer` (`ipe`) concat-dedups
        // every array except `fallbackModel`, and both describe strings say
        // "Arrays merge across settings sources (same semantics as
        // allowedMcpServers)."
        allowed_http_hook_urls: concat_dedup(
            prev.allowed_http_hook_urls,
            next.allowed_http_hook_urls,
        ),
        http_hook_allowed_env_vars: concat_dedup(
            prev.http_hook_allowed_env_vars,
            next.http_hook_allowed_env_vars,
        ),
        // Enterprise login/version managed-policy keys (H-BIN-09) — all
        // scalar-override (later source wins); none is a concat/deep-merge
        // field (CC `settingsMergeCustomizer` special-cases only specific
        // arrays/objects, and none of these is one).
        force_login_method: next.force_login_method.or(prev.force_login_method),
        force_login_gateway_url: next
            .force_login_gateway_url
            .or(prev.force_login_gateway_url),
        force_login_org_uuid: next.force_login_org_uuid.or(prev.force_login_org_uuid),
        parent_settings_behavior: next
            .parent_settings_behavior
            .or(prev.parent_settings_behavior),
        minimum_version: next.minimum_version.or(prev.minimum_version),
        required_minimum_version: next
            .required_minimum_version
            .or(prev.required_minimum_version),
        required_maximum_version: next
            .required_maximum_version
            .or(prev.required_maximum_version),
        force_remote_settings_refresh: next
            .force_remote_settings_refresh
            .or(prev.force_remote_settings_refresh),
        // `companyAnnouncements` — array-merge (concat-dedup), matching CC's
        // `settingsMergeCustomizer` array customizer.
        company_announcements: concat_dedup(prev.company_announcements, next.company_announcements),
        // `plansDirectory` / `apiKeyHelper` — scalar Override (later source wins),
        // same as `model`/`outputStyle`.
        plans_directory: next.plans_directory.or(prev.plans_directory),
        api_key_helper: next.api_key_helper.or(prev.api_key_helper),
        vim_insert_mode_remaps: merge_string_map(
            prev.vim_insert_mode_remaps,
            next.vim_insert_mode_remaps,
        ),
        // 2.1.207 `otelHeadersHelper` (H-BIN-06) — plain string, scalar Override.
        otel_headers_helper: next.otel_headers_helper.or(prev.otel_headers_helper),
        enabled_plugins: deep_merge_object(prev.enabled_plugins, next.enabled_plugins),
        plugin_configs: deep_merge_object(prev.plugin_configs, next.plugin_configs),
        extra_known_marketplaces: deep_merge_object(
            prev.extra_known_marketplaces,
            next.extra_known_marketplaces,
        ),
        strict_known_marketplaces: next
            .strict_known_marketplaces
            .or(prev.strict_known_marketplaces),
        blocked_marketplaces: next.blocked_marketplaces.or(prev.blocked_marketplaces),
        provider_region: next.provider_region.or(prev.provider_region),
        providers: deep_merge_object(prev.providers, next.providers),
        routing: deep_merge_value_opt(prev.routing, next.routing),
        fusion: merge_fusion_settings(prev.fusion, next.fusion),
    }
}

fn merge_model_settings(
    prev: Option<std::collections::BTreeMap<String, crate::settings::schema::ModelSettings>>,
    next: Option<std::collections::BTreeMap<String, crate::settings::schema::ModelSettings>>,
) -> Option<std::collections::BTreeMap<String, crate::settings::schema::ModelSettings>> {
    match (prev, next) {
        (None, other) | (other, None) => other,
        (Some(mut prev), Some(next)) => {
            for (key, nv) in next {
                prev.entry(key)
                    .and_modify(|pv| {
                        if nv.effort_level.is_some() {
                            pv.effort_level.clone_from(&nv.effort_level);
                        }
                        if nv.max_effort_level.is_some() {
                            pv.max_effort_level.clone_from(&nv.max_effort_level);
                        }
                    })
                    .or_insert(nv);
            }
            Some(prev)
        }
    }
}

fn merge_fusion_settings(
    prev: Option<crate::settings::schema::FusionSettingsJson>,
    next: Option<crate::settings::schema::FusionSettingsJson>,
) -> Option<crate::settings::schema::FusionSettingsJson> {
    match (prev, next) {
        (None, None) => None,
        (Some(v), None) | (None, Some(v)) => Some(v),
        (Some(p), Some(n)) => {
            let pv = serde_json::to_value(&p).unwrap_or(serde_json::Value::Null);
            let nv = serde_json::to_value(&n).unwrap_or(serde_json::Value::Null);
            serde_json::from_value(deep_merge_value(pv, nv)).ok()
        }
    }
}

/// Fold one RAW settings layer over `prev`, applying the SAME per-field merge
/// strategies [`merge`] applies to the typed [`SettingsJson`].
///
/// Exists because a consumer can need the merged view of a settings map it
/// must not round-trip through [`SettingsJson`]: the desktop settings snapshot
/// (`bridge-server`'s `settings_bridge::build_snapshot`) shows the user every
/// key their settings files contain, while `SettingsJson` tolerates-and-
/// IGNORES keys it does not declare, so a typed round-trip would silently drop
/// them. Without this function that consumer had to re-derive the merge rules,
/// and the re-derivation was a flat last-layer-wins overwrite — a value the
/// running engine never resolves.
///
/// One source of truth, twice over: which strategy a key takes comes from
/// [`crate::settings::schema::strategy_for`] (the same table `merge`'s
/// field-by-field code follows — the two are pinned together by
/// `tests::raw_layer_merge_agrees_with_the_typed_merge`), and the combining is
/// done by the very [`concat_dedup`] / [`deep_merge_value`] the typed merge
/// calls.
///
/// A key absent from the strategy table takes [`MergeStrategy::Override`] —
/// `next` wins — which is both what `merge` does for every scalar field and
/// what the schema documents as the default for an unregistered key.
///
/// A `next` value of JSON `null` is treated as ABSENT, not as a value. Every
/// field of [`SettingsJson`] is an `Option`, so `"model": null` deserializes
/// to `None` and the typed merge's `next.or(prev)` keeps the lower layer;
/// writing the null through instead would put a value in the result that the
/// engine never resolves, under this layer's name.
///
/// # Returns
///
/// The keys whose merged value is a genuine CROSS-LAYER union: the result
/// differs from `next`'s own value, so no single layer's value is what the
/// merge produced, and a caller reporting provenance must not name one layer
/// for them. Keys that collapse to `next` — every `Override` key, and a deep
/// merge whose entries `next` all redefines — are NOT listed: for those the
/// value shown really is that one layer's, and naming it is honest.
#[must_use]
pub fn merge_raw_layer(
    prev: &mut std::collections::BTreeMap<String, serde_json::Value>,
    next: std::collections::BTreeMap<String, serde_json::Value>,
) -> Vec<String> {
    use serde_json::Value;

    let mut unioned = Vec::new();
    for (key, v_next) in next {
        if v_next.is_null() {
            // Absent, not a value — see this function's doc. `null` shadows
            // nothing, so `prev` keeps whatever it had and a null no layer
            // overrides simply leaves the key unset.
            continue;
        }
        let Some(v_prev) = prev.remove(&key) else {
            // Only one layer defines it — nothing to merge, nothing to report.
            prev.insert(key, v_next);
            continue;
        };
        let merged = match strategy_for(&key).unwrap_or(MergeStrategy::Override) {
            MergeStrategy::Override => v_next.clone(),
            MergeStrategy::ConcatDedup => match (v_prev, v_next.clone()) {
                (Value::Array(p), Value::Array(n)) => {
                    // `unwrap_or_default` is unreachable: both sides are
                    // `Some`, so `concat_dedup` always returns `Some`.
                    Value::Array(concat_dedup(Some(p), Some(n)).unwrap_or_default())
                }
                // A non-array on either side cannot be concatenated, and the
                // typed merge would not even have parsed it. `next` wins, as it
                // does for every shape mismatch in `deep_merge_value`.
                (_, n) => n,
            },
            // Non-objects fall through `deep_merge_value`'s own mismatch arm to
            // `next`, matching `deep_merge_object` / `deep_merge_value_opt`.
            MergeStrategy::DeepMerge => deep_merge_value(v_prev, v_next.clone()),
        };
        if merged != v_next {
            unioned.push(key.clone());
        }
        prev.insert(key, merged);
    }
    unioned
}

/// Concatenate `prev` then append from `next`, dropping duplicates while
/// preserving first-seen order. Matches spec §7 `ConcatDedup` semantics.
///
/// Generic over the element type so the typed merge above (`Vec<String>`) and
/// [`merge_raw_layer`] (`Vec<serde_json::Value>`) run the SAME concatenation,
/// rather than one of them growing a look-alike copy that can drift.
/// `PartialEq` is all the dedup needs.
fn concat_dedup<T: PartialEq>(prev: Option<Vec<T>>, next: Option<Vec<T>>) -> Option<Vec<T>> {
    match (prev, next) {
        (None, None) => None,
        (Some(v), None) | (None, Some(v)) => Some(v),
        (Some(p), Some(n)) => {
            let mut out: Vec<T> = Vec::with_capacity(p.len() + n.len());
            for s in p.into_iter().chain(n.into_iter()) {
                if !out.contains(&s) {
                    out.push(s);
                }
            }
            Some(out)
        }
    }
}

/// Deep-merge two flat `String→String` maps (`modelOverrides`): union of keys,
/// `next` wins on a collision. A one-level record has no nested structure, so
/// this is CC's lodash object-merge for `modelOverrides`.
fn merge_string_map(
    prev: Option<std::collections::BTreeMap<String, String>>,
    next: Option<std::collections::BTreeMap<String, String>>,
) -> Option<std::collections::BTreeMap<String, String>> {
    match (prev, next) {
        (None, None) => None,
        (Some(v), None) | (None, Some(v)) => Some(v),
        (Some(mut p), Some(n)) => {
            for (k, v) in n {
                p.insert(k, v);
            }
            Some(p)
        }
    }
}

/// Deep-merge two object-shaped fields.
///
/// Both sides None → None. One side None → the other side. Both sides Some
/// → key-by-key merge: matching nested objects recurse via [`deep_merge_value`];
/// scalar / array / mismatched-shape keys take `next` (later source wins).
fn deep_merge_object(
    prev: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    next: Option<std::collections::BTreeMap<String, serde_json::Value>>,
) -> Option<std::collections::BTreeMap<String, serde_json::Value>> {
    match (prev, next) {
        (None, None) => None,
        (Some(v), None) | (None, Some(v)) => Some(v),
        (Some(mut p), Some(n)) => {
            for (k, v_next) in n {
                match p.remove(&k) {
                    Some(v_prev) => {
                        p.insert(k, deep_merge_value(v_prev, v_next));
                    }
                    None => {
                        p.insert(k, v_next);
                    }
                }
            }
            Some(p)
        }
    }
}

/// Deep-merge two `Option<serde_json::Value>` fields.
///
/// Both sides `None` → `None`. One side `None` → the other side. Both sides
/// `Some` → recurse via [`deep_merge_value`] (object keys merged; scalar/array
/// mismatch takes `next`).
fn deep_merge_value_opt(
    prev: Option<serde_json::Value>,
    next: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    match (prev, next) {
        (None, None) => None,
        (Some(v), None) | (None, Some(v)) => Some(v),
        (Some(p), Some(n)) => Some(deep_merge_value(p, n)),
    }
}

/// Recurse into a single JSON node. Matches deep-merge semantics for two-level
/// nesting (e.g. `hooks.PreToolUse.{Bash,Read}` from the test).
fn deep_merge_value(prev: serde_json::Value, next: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match (prev, next) {
        (Value::Object(mut p), Value::Object(n)) => {
            for (k, v_next) in n {
                match p.remove(&k) {
                    Some(v_prev) => {
                        p.insert(k, deep_merge_value(v_prev, v_next));
                    }
                    None => {
                        p.insert(k, v_next);
                    }
                }
            }
            Value::Object(p)
        }
        // Mismatched shape or non-object — next wins (Override semantics).
        (_, next) => next,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_push_notification_setting_is_scalar_override() {
        let merged = merge(
            SettingsJson {
                agent_push_notif_enabled: Some(false),
                ..Default::default()
            },
            SettingsJson {
                agent_push_notif_enabled: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(merged.agent_push_notif_enabled, Some(true));
    }
    use crate::settings::schema::SettingsJson;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[test]
    fn concat_dedup_preserves_first_seen_order() {
        let prev = SettingsJson {
            trusted_directories: Some(vec![s("/a"), s("/b"), s("/c")]),
            ..Default::default()
        };
        let next = SettingsJson {
            trusted_directories: Some(vec![s("/b"), s("/d"), s("/a")]),
            ..Default::default()
        };
        let merged = merge(prev, next);
        assert_eq!(
            merged.trusted_directories.as_deref(),
            Some(&[s("/a"), s("/b"), s("/c"), s("/d")][..]),
            "expected prev order kept and only new entries appended"
        );
    }

    #[test]
    fn concat_dedup_handles_one_side_none() {
        let prev = SettingsJson {
            trusted_directories: Some(vec![s("/a")]),
            ..Default::default()
        };
        let next = SettingsJson::default();
        let merged = merge(prev, next);
        assert_eq!(merged.trusted_directories.as_deref(), Some(&[s("/a")][..]));
    }

    #[test]
    fn concat_dedup_covers_all_four_array_fields() {
        let prev = SettingsJson {
            additional_directories: Some(vec![s("/x")]),
            enabled_tools: Some(vec![s("Bash")]),
            additional_includes: Some(vec![s("LINGXI.md")]),
            ..Default::default()
        };
        let next = SettingsJson {
            additional_directories: Some(vec![s("/y")]),
            enabled_tools: Some(vec![s("Read")]),
            additional_includes: Some(vec![s("AGENTS.md")]),
            ..Default::default()
        };
        let merged = merge(prev, next);
        assert_eq!(
            merged.additional_directories.as_deref(),
            Some(&[s("/x"), s("/y")][..])
        );
        assert_eq!(
            merged.enabled_tools.as_deref(),
            Some(&[s("Bash"), s("Read")][..])
        );
        assert_eq!(
            merged.additional_includes.as_deref(),
            Some(&[s("LINGXI.md"), s("AGENTS.md")][..])
        );
    }

    #[test]
    fn object_deep_merge_recurses_one_level() {
        use serde_json::json;
        use std::collections::BTreeMap;

        let mut prev_sandbox = BTreeMap::new();
        prev_sandbox.insert("enabled".to_string(), json!(true));
        prev_sandbox.insert("failIfUnavailable".to_string(), json!(false));

        let mut next_sandbox = BTreeMap::new();
        next_sandbox.insert("failIfUnavailable".to_string(), json!(true));
        next_sandbox.insert(
            "network".to_string(),
            json!({"allowedDomains": ["github.com"]}),
        );

        let prev = SettingsJson {
            sandbox: Some(prev_sandbox),
            ..Default::default()
        };
        let next = SettingsJson {
            sandbox: Some(next_sandbox),
            ..Default::default()
        };

        let merged = merge(prev, next);
        let s = merged.sandbox.unwrap();
        assert_eq!(
            s.get("enabled"),
            Some(&json!(true)),
            "prev-only key survives"
        );
        assert_eq!(
            s.get("failIfUnavailable"),
            Some(&json!(true)),
            "next overrides scalar"
        );
        assert_eq!(
            s.get("network"),
            Some(&json!({"allowedDomains": ["github.com"]})),
            "next-only key is added"
        );
    }

    #[test]
    fn object_deep_merge_recurses_two_levels() {
        use serde_json::json;
        use std::collections::BTreeMap;

        let mut prev_hooks = BTreeMap::new();
        prev_hooks.insert("PreToolUse".to_string(), json!({"Bash": ["echo prev"]}));

        let mut next_hooks = BTreeMap::new();
        next_hooks.insert("PreToolUse".to_string(), json!({"Read": ["echo next"]}));

        let prev = SettingsJson {
            hooks: Some(prev_hooks),
            ..Default::default()
        };
        let next = SettingsJson {
            hooks: Some(next_hooks),
            ..Default::default()
        };

        let merged = merge(prev, next);
        let h = merged.hooks.unwrap();
        // Inner objects are deep-merged: both PreToolUse subkeys land.
        assert_eq!(
            h.get("PreToolUse"),
            Some(&json!({"Bash": ["echo prev"], "Read": ["echo next"]}))
        );
    }

    #[test]
    fn scalar_override_next_wins_when_set() {
        let prev = SettingsJson {
            model: Some("sonnet".into()),
            telemetry_enabled: Some(false),
            enable_workflows: Some(true),
            workflow_size_guideline: Some("small".into()),
            ..Default::default()
        };
        let next = SettingsJson {
            model: Some("opus".into()),
            telemetry_enabled: None,
            enable_workflows: Some(false),
            workflow_size_guideline: Some("large".into()),
            ..Default::default()
        };
        let merged = merge(prev, next);
        assert_eq!(merged.model.as_deref(), Some("opus"));
        assert_eq!(
            merged.telemetry_enabled,
            Some(false),
            "next is None, so prev survives"
        );
        assert_eq!(
            merged.enable_workflows,
            Some(false),
            "enableWorkflows is scalar-override"
        );
        assert_eq!(
            merged.workflow_size_guideline.as_deref(),
            Some("large"),
            "workflowSizeGuideline is scalar-override"
        );
    }

    #[test]
    fn enterprise_login_version_keys_scalar_override() {
        // H-BIN-09: all enterprise login/version keys are scalar-override —
        // the higher-priority (`next`) layer wins when it sets the key, else the
        // lower layer survives. Models the 4-layer stack folding a base managed
        // tier under a higher-priority drop-in.
        use serde_json::json;
        let prev = SettingsJson {
            force_login_method: Some("claudeai".into()),
            required_minimum_version: Some("2.0.0".into()),
            force_login_org_uuid: Some(json!("org-base")),
            parent_settings_behavior: Some("first-wins".into()),
            force_remote_settings_refresh: Some(false),
            ..Default::default()
        };
        let next = SettingsJson {
            // next overrides method + min + org pin; leaves the rest unset.
            force_login_method: Some("gateway".into()),
            required_minimum_version: Some("2.1.207".into()),
            force_login_org_uuid: Some(json!(["org-a", "org-b"])),
            ..Default::default()
        };
        let merged = merge(prev, next);
        assert_eq!(merged.force_login_method.as_deref(), Some("gateway"));
        assert_eq!(merged.required_minimum_version.as_deref(), Some("2.1.207"));
        assert_eq!(
            merged.force_login_org_uuid,
            Some(json!(["org-a", "org-b"])),
            "org pin is scalar-override (source array wins, not concat)"
        );
        // Keys next left unset survive from prev.
        assert_eq!(
            merged.parent_settings_behavior.as_deref(),
            Some("first-wins")
        );
        assert_eq!(merged.force_remote_settings_refresh, Some(false));
    }

    #[test]
    fn http_hook_security_keys_concat_dedup_across_tiers() {
        // H-BIN-12: both HTTP-hook allowlists concat-dedup across settings
        // sources (CC `settingsMergeCustomizer` concat-dedups every array except
        // `fallbackModel`). A pattern/env-var declared in the lower tier survives
        // and the higher tier's entries append (deduped).
        let prev = SettingsJson {
            allowed_http_hook_urls: Some(vec![s("https://a.example.com/*"), s("https://shared/*")]),
            http_hook_allowed_env_vars: Some(vec![s("TOKEN_A"), s("SHARED")]),
            ..Default::default()
        };
        let next = SettingsJson {
            allowed_http_hook_urls: Some(vec![s("https://shared/*"), s("https://b.example.com/*")]),
            http_hook_allowed_env_vars: Some(vec![s("SHARED"), s("TOKEN_B")]),
            ..Default::default()
        };
        let merged = merge(prev, next);
        assert_eq!(
            merged.allowed_http_hook_urls.as_deref(),
            Some(
                &[
                    s("https://a.example.com/*"),
                    s("https://shared/*"),
                    s("https://b.example.com/*")
                ][..]
            )
        );
        assert_eq!(
            merged.http_hook_allowed_env_vars.as_deref(),
            Some(&[s("TOKEN_A"), s("SHARED"), s("TOKEN_B")][..])
        );
    }

    #[test]
    fn providers_deep_merge_combines_profiles() {
        use serde_json::json;
        use std::collections::BTreeMap;
        let mut p = BTreeMap::new();
        p.insert("groq".to_string(), json!({"type": "openai"}));
        let mut n = BTreeMap::new();
        n.insert("ollama".to_string(), json!({"type": "openai"}));
        let prev = SettingsJson {
            providers: Some(p),
            ..Default::default()
        };
        let next = SettingsJson {
            providers: Some(n),
            ..Default::default()
        };
        let merged = merge(prev, next).providers.unwrap();
        assert!(merged.contains_key("groq") && merged.contains_key("ollama"));
    }

    #[test]
    fn fusion_completion_policy_cross_layer_conflict_is_preserved_for_validation() {
        let lower: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"completionPolicy":"quorum_after_grace"}}"#)
                .unwrap();
        let upper: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"partialOk":false}}"#).unwrap();
        lower.fusion.as_ref().unwrap().validate().unwrap();
        upper.fusion.as_ref().unwrap().validate().unwrap();
        let merged = merge(lower, upper);
        let settings = merged.fusion.unwrap();
        assert_eq!(
            settings.completion_policy,
            Some(crate::settings::schema::FusionCompletionPolicy::QuorumAfterGrace)
        );
        assert!(settings.validate().is_err());
        let reset: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"completionPolicy":"wait_all"}}"#).unwrap();
        let merged = merge(
            SettingsJson {
                fusion: Some(settings),
                ..Default::default()
            },
            reset,
        );
        merged.fusion.unwrap().validate().unwrap();
    }

    #[test]
    fn fusion_deep_merge_overrides_scalars_and_replaces_arrays() {
        let prev = SettingsJson {
            fusion: Some(crate::settings::schema::FusionSettingsJson {
                enabled: Some(false),
                quality_panel_count: Some(3),
                allowed_profiles: Some(vec!["anthropic".into()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let next = SettingsJson {
            fusion: Some(crate::settings::schema::FusionSettingsJson {
                enabled: Some(true),
                allowed_profiles: Some(vec!["openai".into()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = merge(prev, next).fusion.unwrap();
        assert_eq!(merged.enabled, Some(true));
        assert_eq!(merged.quality_panel_count, Some(3));
        assert_eq!(
            merged.allowed_profiles.as_deref(),
            Some(&["openai".to_string()][..])
        );
    }

    #[test]
    fn new_238_settings_fields_merge_with_object_and_scalar_semantics() {
        use serde_json::json;
        use std::collections::BTreeMap;

        let mut prev_policy_helpers = BTreeMap::new();
        prev_policy_helpers.insert(
            "defaultSettings".to_string(),
            json!({"command": "/usr/bin/base-helper"}),
        );
        let mut next_policy_helpers = BTreeMap::new();
        next_policy_helpers.insert(
            "darwin".to_string(),
            json!({"command": "/opt/darwin-helper"}),
        );

        let mut prev_spellcheck = BTreeMap::new();
        prev_spellcheck.insert("enabled".to_string(), json!(true));
        let mut next_spellcheck = BTreeMap::new();
        next_spellcheck.insert("checkFilenames".to_string(), json!(false));

        let mut prev_additional_marketplaces = BTreeMap::new();
        prev_additional_marketplaces.insert(
            "corp".to_string(),
            json!({ "source": { "source": "directory", "path": "/tmp/corp" } }),
        );
        let mut next_additional_marketplaces = BTreeMap::new();
        next_additional_marketplaces.insert(
            "beta".to_string(),
            json!({ "source": { "source": "directory", "path": "/tmp/beta" } }),
        );

        let merged = merge(
            SettingsJson {
                policy_helpers: Some(prev_policy_helpers),
                sync_claude_ai_skills: Some(false),
                additional_marketplaces: Some(prev_additional_marketplaces),
                allowed_marketplaces: Some(vec![json!("corp"), json!("stable")]),
                disable_command_plugin_sources: Some(false),
                spellcheck: Some(prev_spellcheck),
                model_proposed_goals: Some("auto".into()),
                keybinding_flavor: Some("classic".into()),
                auto_continue_at_usage_limit: Some(false),
                ..Default::default()
            },
            SettingsJson {
                policy_helpers: Some(next_policy_helpers),
                sync_claude_ai_skills: Some(true),
                additional_marketplaces: Some(next_additional_marketplaces),
                allowed_marketplaces: None,
                disable_command_plugin_sources: Some(true),
                spellcheck: Some(next_spellcheck),
                model_proposed_goals: Some("alwaysAsk".into()),
                keybinding_flavor: Some("readline".into()),
                auto_continue_at_usage_limit: Some(true),
                ..Default::default()
            },
        );

        let policy_helpers = merged.policy_helpers.expect("policyHelpers");
        assert_eq!(
            policy_helpers.get("defaultSettings"),
            Some(&json!({"command": "/usr/bin/base-helper"}))
        );
        assert_eq!(
            policy_helpers.get("darwin"),
            Some(&json!({"command": "/opt/darwin-helper"}))
        );

        let spellcheck = merged.spellcheck.expect("spellcheck");
        assert_eq!(spellcheck.get("enabled"), Some(&json!(true)));
        assert_eq!(spellcheck.get("checkFilenames"), Some(&json!(false)));

        assert_eq!(merged.sync_claude_ai_skills, Some(true));
        let additional_marketplaces = merged
            .additional_marketplaces
            .expect("additional marketplaces");
        assert_eq!(
            additional_marketplaces.get("corp"),
            Some(&json!({ "source": { "source": "directory", "path": "/tmp/corp" } }))
        );
        assert_eq!(
            additional_marketplaces.get("beta"),
            Some(&json!({ "source": { "source": "directory", "path": "/tmp/beta" } }))
        );
        assert_eq!(
            merged.allowed_marketplaces,
            Some(vec![json!("corp"), json!("stable")])
        );
        assert_eq!(merged.disable_command_plugin_sources, Some(true));
        assert_eq!(merged.model_proposed_goals.as_deref(), Some("alwaysAsk"));
        assert_eq!(merged.keybinding_flavor.as_deref(), Some("readline"));
        assert_eq!(merged.auto_continue_at_usage_limit, Some(true));
    }

    #[test]
    fn emoji_completion_enabled_parses_and_higher_layer_wins() {
        let parsed: SettingsJson =
            serde_json::from_str(r#"{"emojiCompletionEnabled":false}"#).unwrap();
        assert_eq!(parsed.emoji_completion_enabled, Some(false));

        let merged = merge(
            SettingsJson {
                emoji_completion_enabled: Some(true),
                ..Default::default()
            },
            parsed,
        );
        assert_eq!(merged.emoji_completion_enabled, Some(false));
        assert!(serde_json::to_string(&merged)
            .unwrap()
            .contains("\"emojiCompletionEnabled\":false"));
    }

    #[test]
    fn show_thinking_summaries_parses_and_higher_layer_wins() {
        let parsed: SettingsJson =
            serde_json::from_str(r#"{"showThinkingSummaries":true}"#).unwrap();
        assert_eq!(parsed.show_thinking_summaries, Some(true));

        let merged = merge(
            SettingsJson {
                show_thinking_summaries: Some(false),
                ..Default::default()
            },
            parsed,
        );
        assert_eq!(merged.show_thinking_summaries, Some(true));
        assert!(serde_json::to_string(&merged)
            .unwrap()
            .contains("\"showThinkingSummaries\":true"));
    }

    #[test]
    fn vision_delegation_parses_and_higher_layer_wins() {
        let parsed: SettingsJson =
            serde_json::from_str(r#"{"visionDelegationEnabled":false}"#).unwrap();
        assert_eq!(parsed.vision_delegation_enabled, Some(false));

        let merged = merge(
            SettingsJson {
                vision_delegation_enabled: Some(true),
                ..Default::default()
            },
            parsed,
        );
        assert_eq!(merged.vision_delegation_enabled, Some(false));
        assert!(serde_json::to_string(&merged)
            .unwrap()
            .contains("\"visionDelegationEnabled\":false"));
    }

    /// (key, lower layer's value, upper layer's value) for every key the
    /// strategy table registers, plus a few `Override` keys as the contrast.
    ///
    /// Every pair DIFFERS between the two layers on purpose: a fixture where
    /// both layers say the same thing agrees under every strategy and proves
    /// nothing. Shared by the two tests below so "is this key covered" has one
    /// answer for both directions of the drift check.
    fn merge_fixture_cases() -> Vec<(&'static str, serde_json::Value, serde_json::Value)> {
        use serde_json::json;

        vec![
            ("trustedDirectories", json!(["/a"]), json!(["/b"])),
            ("additionalDirectories", json!(["/c"]), json!(["/d"])),
            ("enabledTools", json!(["Bash"]), json!(["Read"])),
            ("additionalIncludes", json!(["a.md"]), json!(["b.md"])),
            ("lingxiMdExcludes", json!(["x/**"]), json!(["y/**"])),
            ("companyAnnouncements", json!(["one"]), json!(["two"])),
            // Each side names a DIFFERENT field, so the typed and raw paths
            // only agree if both deep-merge per field — an override on either
            // side would drop one of the two and show up as a mismatch.
            (
                "attribution",
                json!({"commit": "from lower"}),
                json!({"pr": "from upper"}),
            ),
            (
                "allowedHttpHookUrls",
                json!(["https://a"]),
                json!(["https://b"]),
            ),
            ("httpHookAllowedEnvVars", json!(["A"]), json!(["B"])),
            ("sandbox", json!({"lower": 1}), json!({"upper": 2})),
            (
                "hooks",
                json!({"PreToolUse": {"Bash": "lower"}}),
                json!({"PostToolUse": {"Read": "upper"}}),
            ),
            (
                "permissions",
                json!({"allow": ["Bash(ls)"]}),
                json!({"deny": ["Bash(rm)"]}),
            ),
            (
                "policyHelpers",
                json!({"macos": "l"}),
                json!({"linux": "u"}),
            ),
            (
                "spellcheck",
                json!({"lower": true}),
                json!({"upper": false}),
            ),
            (
                "additionalMarketplaces",
                json!({"lower": {"source": "l"}}),
                json!({"upper": {"source": "u"}}),
            ),
            (
                "enabledPlugins",
                json!({"lower@m": true}),
                json!({"upper@m": true}),
            ),
            ("pluginConfigs", json!({"lower": {}}), json!({"upper": {}})),
            (
                "extraKnownMarketplaces",
                json!({"lower": {"source": {"source": "github"}}}),
                json!({"upper": {"source": {"source": "github"}}}),
            ),
            (
                "providers",
                json!({"lowerProfile": {"type": "openai"}}),
                json!({"upperProfile": {"type": "openai"}}),
            ),
            (
                "routing",
                json!({"aliases": {"lower": "a"}}),
                json!({"retry": {"maxAttempts": 3}}),
            ),
            (
                "fusion",
                json!({"qualityPanelCount": 3}),
                json!({"fastPanelCount": 2}),
            ),
            (
                "modelSettings",
                json!({"claude-opus-4-7": {"maxEffortLevel": "high"}}),
                json!({"claude-sonnet-4-5": {"effortLevel": "medium"}}),
            ),
            (
                "modelOverrides",
                json!({"lower": "m1"}),
                json!({"upper": "m2"}),
            ),
            (
                "vimInsertModeRemaps",
                json!({"jj": "Escape"}),
                json!({"kk": "Escape"}),
            ),
            // Not in the table — the default `Override` must survive the
            // round trip too, or "everything unions" would pass this test.
            ("outputStyle", json!("lower"), json!("upper")),
            ("model", json!("m-lower"), json!("m-upper")),
            ("availableModels", json!(["lower"]), json!(["upper"])),
        ]
    }

    /// The strategy TABLE (`schema::MERGE_STRATEGIES`) and the field-by-field
    /// typed `merge` above are two spellings of one set of rules, and
    /// [`merge_raw_layer`] reads the table. If they disagree, every consumer
    /// of the table reports a merge the engine does not perform, silently.
    /// `merge_raw_layer` (and so the desktop settings snapshot) is wrong for
    /// ANY disagreement; `tracer` collapses `DeepMerge` and `Override` into
    /// the same branch, so only a `ConcatDedup` disagreement reaches it.
    ///
    /// This is the TABLE ⇒ FIXTURE direction only: a registered key with no
    /// fixture fails here. The MERGE ⇒ TABLE direction — the one the
    /// `vimInsertModeRemaps` drift actually travelled, a field `merge`
    /// combines with NO table entry — is
    /// `every_field_merge_combines_is_registered_and_fixtured`. Neither
    /// direction alone catches both, and this one alone would have gone green
    /// on the very drift it was written after.
    #[test]
    fn raw_layer_merge_agrees_with_the_typed_merge() {
        use crate::settings::schema::MERGE_STRATEGIES;

        let cases = merge_fixture_cases();

        for (key, _) in MERGE_STRATEGIES {
            assert!(
                cases.iter().any(|(k, _, _)| k == key),
                "MERGE_STRATEGIES entry {key:?} has no case in `merge_fixture_cases`, so this \
                 test does not check it — add a (lower, upper) pair for it"
            );
        }

        let lower: std::collections::BTreeMap<String, serde_json::Value> = cases
            .iter()
            .map(|(k, l, _)| ((*k).to_string(), l.clone()))
            .collect();
        let upper: std::collections::BTreeMap<String, serde_json::Value> = cases
            .iter()
            .map(|(k, _, u)| ((*k).to_string(), u.clone()))
            .collect();

        // The typed path: exactly what `Settings::load` does per layer.
        let typed_lower: SettingsJson =
            serde_json::from_value(serde_json::to_value(&lower).unwrap())
                .expect("the fixture must parse as SettingsJson, or it is not testing `merge`");
        let typed_upper: SettingsJson =
            serde_json::from_value(serde_json::to_value(&upper).unwrap())
                .expect("the fixture must parse as SettingsJson, or it is not testing `merge`");
        let typed_merged: std::collections::BTreeMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::to_value(merge(typed_lower, typed_upper)).unwrap())
                .unwrap();

        // The raw path: what a consumer that must keep unknown keys does.
        // BOTH layers go through `merge_raw_layer`, starting from an EMPTY
        // accumulator, because that is what `build_snapshot` does — seeding
        // the accumulator with `lower` directly would skip the function under
        // test for the first layer and hide whatever it does on the way in.
        let mut raw_merged = std::collections::BTreeMap::new();
        // The first layer's union report is not what this case asserts on —
        // `unioned` below (the SECOND layer's) is. Discard it explicitly so
        // the `#[must_use]` is answered rather than warned about.
        let _first_layer_union = merge_raw_layer(&mut raw_merged, lower.clone());
        let unioned = merge_raw_layer(&mut raw_merged, upper.clone());

        for (key, _, _) in &cases {
            assert_eq!(
                raw_merged.get(*key),
                typed_merged.get(*key),
                "`merge_raw_layer` and `merge` disagree on {key:?}"
            );
        }

        // The union report must name the merged keys and ONLY them: a report
        // that listed every key would satisfy any "is it in there" assertion
        // while telling a caller nothing.
        assert!(
            unioned.contains(&"hooks".to_string()),
            "a deep-merged key defined in both layers is a cross-layer union, got {unioned:?}"
        );
        assert!(
            unioned.contains(&"trustedDirectories".to_string()),
            "a concat-dedup key defined in both layers is a cross-layer union, got {unioned:?}"
        );
        assert!(
            unioned.contains(&"vimInsertModeRemaps".to_string()),
            "the drift this test was written to catch: `vimInsertModeRemaps` unions, got {unioned:?}"
        );
        for key in ["outputStyle", "model", "availableModels"] {
            assert!(
                !unioned.contains(&key.to_string()),
                "{key:?} is scalar-override — its value IS the upper layer's, so reporting it \
                 as merged would be the same lie in the other direction, got {unioned:?}"
            );
        }
    }

    /// The MERGE ⇒ TABLE direction, and the reason this file carries two
    /// drift tests rather than one.
    ///
    /// `raw_layer_merge_agrees_with_the_typed_merge` asserts "every TABLE
    /// entry has a fixture". That is not the direction the
    /// `vimInsertModeRemaps` drift travelled: the field was combined by
    /// `merge` with NO table entry at all, so it was absent from the table and
    /// from the fixture alike, and a table-driven loop had nothing to iterate
    /// over. Had the fixture case not been hand-added at the same time, that
    /// test would have passed with the bug sitting in place — a guard green
    /// for precisely the thing it was built to catch.
    ///
    /// So this test walks the other way. Its key set is derived from
    /// `SettingsJson` itself — the schemars property names, which carry the
    /// `rename_all = "camelCase"` serde spelling — and for every field it
    /// MEASURES whether `merge` combines the two layers or just takes `next`.
    /// A field that combines must be registered in `MERGE_STRATEGIES` with a
    /// non-`Override` strategy AND covered by `merge_fixture_cases`. Adding a
    /// `deep_merge_object(...)` / `concat_dedup(...)` field to `merge` and
    /// forgetting the table entry turns this red.
    ///
    /// "Combines" is measured, never declared: the probe pair for a field is
    /// found by trying candidate shapes until one round-trips through
    /// `SettingsJson`, and the field counts as combining when
    /// `merge(lower, upper)` yields something other than `upper`. No list of
    /// which fields deep-merge appears anywhere in this test — such a list
    /// would have to be kept in sync by hand, which is the same decoration in
    /// a new place.
    #[test]
    fn every_field_merge_combines_is_registered_and_fixtured() {
        use crate::settings::schema::{strategy_for, MERGE_STRATEGIES};
        use serde_json::{json, Value};

        // Probe shapes, UNION-CAPABLE ones first: an object or an array can
        // reveal a deep merge or a concat; a scalar cannot. Order matters —
        // `routing` is `Option<Value>` and accepts every shape, so trying a
        // scalar first would make a genuinely deep-merged field look trivial.
        // The two sides always differ, and never by being absent: `true`/
        // `false` rather than `false`/`true`, so an `Override` field's result
        // is `Some(false)` and not confusable with "field unset".
        let candidates: Vec<(Value, Value)> = vec![
            (json!({"lowerEntry": "l"}), json!({"upperEntry": "u"})),
            (
                json!({"qualityPanelCount": 3}),
                json!({"fastPanelCount": 2}),
            ),
            // `attribution` is a TYPED object, so the generic `lowerEntry`/
            // `upperEntry` probe cannot round-trip it — serde drops the unknown
            // keys and the re-serialized value no longer matches. It needs a
            // pair drawn from its own fields, and the two sides must name
            // DIFFERENT ones so a per-field merge is visible as combining.
            (json!({"commit": "lower"}), json!({"pr": "upper"})),
            (
                json!({"claude-opus-4-7": {"maxEffortLevel": "high"}}),
                json!({"claude-sonnet-4-5": {"effortLevel": "medium"}}),
            ),
            (json!(["lower"]), json!(["upper"])),
            (json!("lower"), json!("upper")),
            (json!(true), json!(false)),
            (json!(1), json!(2)),
            // String ENUMS reject the generic `"lower"`/`"upper"` pair, so they
            // need real variants. Appended last on purpose: the search takes the
            // first shape that round-trips, so this can only ever be reached by a
            // field for which every shape above already failed — which is the
            // case that used to panic.
            (json!("auto"), json!("tmux")),
        ];

        // Whether `{field: value}` survives a `SettingsJson` round trip
        // unchanged — i.e. whether this probe shape is the field's real type.
        // A wrong-typed known field fails to parse; an unknown field parses
        // but vanishes from the re-serialization, so both are rejected.
        let round_trips = |field: &str, value: &Value| -> bool {
            let Ok(parsed) = serde_json::from_value::<SettingsJson>(json!({ field: value })) else {
                return false;
            };
            serde_json::to_value(&parsed)
                .ok()
                .and_then(|doc| doc.get(field).cloned())
                .as_ref()
                == Some(value)
        };

        let schema = serde_json::to_value(schemars::schema_for!(SettingsJson))
            .expect("the derived JSON schema must serialize");
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("SettingsJson's derived schema must list its properties");
        // A zero-length or truncated loop is the classic false green: it would
        // pass this test while checking nothing. `merge` handles ~70 fields.
        assert!(
            properties.len() > 60,
            "expected the whole settings schema, got {} properties — the enumeration broke and \
             this test is checking almost nothing",
            properties.len()
        );

        let cases = merge_fixture_cases();
        let mut combining = Vec::new();
        for field in properties.keys() {
            let Some((lower, upper)) = candidates
                .iter()
                .find(|(l, u)| round_trips(field, l) && round_trips(field, u))
            else {
                panic!(
                    "no candidate probe shape round-trips through SettingsJson for {field:?}, so \
                     this test cannot tell whether `merge` combines it — add a probe shape that \
                     matches its type"
                );
            };

            let typed_lower: SettingsJson =
                serde_json::from_value(json!({ field: lower })).expect("probe must parse");
            let typed_upper: SettingsJson =
                serde_json::from_value(json!({ field: upper })).expect("probe must parse");
            let merged = serde_json::to_value(merge(typed_lower, typed_upper))
                .expect("a merged SettingsJson must serialize");
            let merged_field = merged.get(field);

            if merged_field == Some(upper) {
                continue; // Plain Override: `next` won outright.
            }
            combining.push(field.clone());

            assert!(
                strategy_for(field).is_some_and(|s| s != MergeStrategy::Override),
                "`merge` COMBINES {field:?} across layers (probe merged to {merged_field:?}, not \
                 to the upper layer's {upper:?}), but MERGE_STRATEGIES has \
                 {:?} for it. Every table reader — `merge_raw_layer`, and so the desktop \
                 settings snapshot — will resolve this field the wrong way. Register it.",
                strategy_for(field)
            );
            assert!(
                cases.iter().any(|(k, _, _)| k == field),
                "`merge` combines {field:?}, but `merge_fixture_cases` has no pair for it, so \
                 `raw_layer_merge_agrees_with_the_typed_merge` never compares the two paths on \
                 it — add one"
            );
        }

        // The scan must actually have FOUND the combining fields; if the probe
        // shapes stopped matching, every field would look like a plain
        // Override and the loop above would assert nothing at all.
        assert_eq!(
            combining.len(),
            MERGE_STRATEGIES.len(),
            "the probe found {} combining fields but the table registers {} — either a probe \
             shape stopped fitting its field (so this test silently checks less than it claims) \
             or the table registers a key `merge` does not actually combine. Found: {combining:?}",
            combining.len(),
            MERGE_STRATEGIES.len()
        );
    }

    /// A JSON `null` is "unset", not "set to null" — and the authority for
    /// that is `merge` itself, which this test compares against rather than
    /// asserting what I believe the answer should be.
    ///
    /// Every `SettingsJson` field is an `Option`, so `"model": null` parses to
    /// `None` and `next.or(prev)` keeps the lower layer. A raw path that wrote
    /// the null through would put a value in `effective` the engine never
    /// resolves — and, in the snapshot, name the layer that "set" it. Both
    /// shapes are checked: a null over a value, and a null nobody shadows.
    #[test]
    fn a_null_layer_value_is_absent_to_the_raw_path_exactly_as_it_is_to_merge() {
        use serde_json::json;

        let typed_merge_of = |lower: serde_json::Value, upper: serde_json::Value| {
            let l: SettingsJson = serde_json::from_value(lower).expect("lower parses");
            let u: SettingsJson = serde_json::from_value(upper).expect("upper parses");
            serde_json::to_value(merge(l, u)).expect("merged serializes")
        };
        let raw_merge_of = |lower: serde_json::Value, upper: serde_json::Value| {
            let mut acc = std::collections::BTreeMap::new();
            // This helper asserts on the ACCUMULATOR, not on the union
            // report; discard both reports explicitly so `#[must_use]` is
            // answered rather than warned about.
            let _lower_union = merge_raw_layer(
                &mut acc,
                serde_json::from_value(lower).expect("lower is an object"),
            );
            let _upper_union = merge_raw_layer(
                &mut acc,
                serde_json::from_value(upper).expect("upper is an object"),
            );
            serde_json::to_value(acc).expect("accumulator serializes")
        };

        // A null in the upper layer must not erase the lower layer's value.
        let lower = json!({"model": "from-lower"});
        let upper = json!({"model": null});
        assert_eq!(
            raw_merge_of(lower.clone(), upper.clone())["model"],
            json!("from-lower"),
            "a null upper layer must leave the lower layer's value standing"
        );
        assert_eq!(
            raw_merge_of(lower.clone(), upper.clone()),
            typed_merge_of(lower, upper),
            "the raw path must agree with `merge` on a shadowing null"
        );

        // A null no layer shadows leaves the key unset, not set-to-null.
        let only_null = json!({"model": null});
        let merged = raw_merge_of(json!({}), only_null.clone());
        assert_eq!(
            merged.get("model"),
            None,
            "an unshadowed null must leave the key absent, not present-and-null, got {merged}"
        );
        assert_eq!(
            merged,
            typed_merge_of(json!({}), only_null),
            "the raw path must agree with `merge` on an unshadowed null"
        );

        // Deep-merge keys take the same rule — the null check runs before the
        // strategy dispatch, so `hooks: null` cannot wipe a lower layer's hooks.
        let lower = json!({"hooks": {"PreToolUse": {"Bash": "l"}}});
        let upper = json!({"hooks": null});
        assert_eq!(
            raw_merge_of(lower.clone(), upper.clone()),
            typed_merge_of(lower, upper),
            "the raw path must agree with `merge` on a null over a deep-merge key"
        );
    }

    /// A deep-merge key whose entries the upper layer entirely redefines is
    /// NOT a cross-layer union: the merged value IS the upper layer's own
    /// value, so naming that layer is honest and the key must not be reported
    /// as merged. Without this, `merged_keys` would degrade into "both layers
    /// mentioned the key", which is not the question the UI asks.
    #[test]
    fn a_fully_shadowed_deep_merge_key_is_not_reported_as_a_union() {
        let mut prev: std::collections::BTreeMap<String, serde_json::Value> =
            std::collections::BTreeMap::new();
        prev.insert(
            "hooks".to_string(),
            serde_json::json!({"PreToolUse": {"Bash": "lower"}}),
        );
        let mut next = std::collections::BTreeMap::new();
        next.insert(
            "hooks".to_string(),
            serde_json::json!({"PreToolUse": {"Bash": "upper"}}),
        );

        let unioned = merge_raw_layer(&mut prev, next);

        assert_eq!(
            prev.get("hooks"),
            Some(&serde_json::json!({"PreToolUse": {"Bash": "upper"}})),
        );
        assert!(
            unioned.is_empty(),
            "the merged value is exactly the upper layer's, so it is not a union, got {unioned:?}"
        );
    }

    /// `attribution` is an OBJECT, so two layers merge PER FIELD. A project
    /// that sets only `pr` must not erase a user-level `commit` — which is
    /// exactly what the `next.or(prev)` used for scalars would have done.
    #[test]
    fn attribution_merges_per_field_rather_than_replacing_the_object() {
        use crate::settings::schema::Attribution;
        let user = SettingsJson {
            attribution: Some(Attribution {
                commit: Some("user commit".into()),
                pr: None,
            }),
            ..Default::default()
        };
        let project = SettingsJson {
            attribution: Some(Attribution {
                commit: None,
                pr: Some("project pr".into()),
            }),
            ..Default::default()
        };
        let merged = merge(user, project).attribution.expect("attribution");
        assert_eq!(
            merged.commit.as_deref(),
            Some("user commit"),
            "the later layer named only `pr`, so `commit` must survive"
        );
        assert_eq!(merged.pr.as_deref(), Some("project pr"));
    }

    /// Within a field the later layer still wins, and an explicit empty string
    /// is a VALUE that overrides — not an absence that falls through.
    #[test]
    fn a_later_layer_overrides_a_named_field_including_with_empty() {
        use crate::settings::schema::Attribution;
        let user = SettingsJson {
            attribution: Some(Attribution {
                commit: Some("user commit".into()),
                pr: None,
            }),
            ..Default::default()
        };
        let project = SettingsJson {
            attribution: Some(Attribution {
                commit: Some(String::new()),
                pr: None,
            }),
            ..Default::default()
        };
        let merged = merge(user, project).attribution.expect("attribution");
        assert_eq!(
            merged.commit.as_deref(),
            Some(""),
            "an explicit empty must override, i.e. disable the trailer"
        );
    }
}
