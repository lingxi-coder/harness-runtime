// lingxi-code/crates/core/src/settings/merger.rs
//! Per-field merge dispatcher.
//!
//! Signature is consume-and-return because every layer is short-lived during
//! [`crate::settings::Settings::load`]. Strategy choice routes through
//! [`crate::settings::schema::strategy_for`] — Task 5 wires up
//! [`MergeStrategy::ConcatDedup`]; Task 6 adds `DeepMerge` + `Override`.

use crate::settings::schema::{strategy_for, MergeStrategy, SettingsJson};
use crate::types::utf16_json::{JsonValueView, Utf16JsonProjection, Utf16JsonProjectionError};

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

pub fn merge(
    prev: SettingsJson,
    next: SettingsJson,
) -> Result<SettingsJson, Utf16JsonProjectionError> {
    Ok(SettingsJson {
        teammate_mode: next.teammate_mode.or(prev.teammate_mode),
        prompt_cache_ttl: next.prompt_cache_ttl.or(prev.prompt_cache_ttl),
        subagent_prompt_cache_ttl: next
            .subagent_prompt_cache_ttl
            .or(prev.subagent_prompt_cache_ttl),
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
        // Permission arrays accumulate across tiers in Claude Code's settings
        // merge. `$.settings.read()` observes this merged field directly.
        permissions: merge_permissions_object(prev.permissions, next.permissions),
        // Scalar fields — Override semantics: next wins when set, else prev.
        // OUTSTYLE.1: `outputStyle` is a string in claude-code and merges
        // scalar-override (settingsMergeCustomizer special-cases only arrays).
        output_style: next.output_style.or(prev.output_style),
        // Scalar Override — Native's `settings.language` response preference.
        language: next.language.or(prev.language),
        // Scalar Override — Native's proxy authentication helper command.
        proxy_auth_helper: next.proxy_auth_helper.or(prev.proxy_auth_helper),
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
        // Scalar Override (later source wins) — Native bundled skill/workflow
        // catalog gate. The consumer supplies the false default when absent.
        disable_bundled_skills: next.disable_bundled_skills.or(prev.disable_bundled_skills),
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
        policy_helper: next.policy_helper.or(prev.policy_helper),
        // The generic Native Settings merge view uses `mergeWith` semantics
        // for this projection. Helper execution selection separately reads
        // the winning admin source document, never this merged view.
        policy_helpers: merge_policy_helpers_source_object(
            prev.policy_helpers,
            next.policy_helpers,
        )?,
        sync_claude_ai_skills: next.sync_claude_ai_skills.or(prev.sync_claude_ai_skills),
        skill_listing_max_desc_chars: next
            .skill_listing_max_desc_chars
            .or(prev.skill_listing_max_desc_chars),
        skill_listing_budget_fraction: next
            .skill_listing_budget_fraction
            .or(prev.skill_listing_budget_fraction),
        // Native skill overrides are a name-to-enum record and lodash-merges
        // the object across settings layers; later values replace only the
        // same exact UTF-16 skill name.
        skill_overrides: deep_merge_skill_overrides(prev.skill_overrides, next.skill_overrides),
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
        fast_mode: next.fast_mode.or(prev.fast_mode),
        prompt_suggestion_enabled: next
            .prompt_suggestion_enabled
            .or(prev.prompt_suggestion_enabled),
        fast_mode_per_session_opt_in: next
            .fast_mode_per_session_opt_in
            .or(prev.fast_mode_per_session_opt_in),
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
        include_code_review_suggestion: next
            .include_code_review_suggestion
            .or(prev.include_code_review_suggestion),
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
        plugin_configs: deep_merge_source_object(prev.plugin_configs, next.plugin_configs)?,
        extra_known_marketplaces: shallow_merge_object(
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
    })
}

fn deep_merge_skill_overrides(
    prev: Option<crate::settings::schema::SkillOverrides>,
    next: Option<crate::settings::schema::SkillOverrides>,
) -> Option<crate::settings::schema::SkillOverrides> {
    match (prev, next) {
        (None, other) | (other, None) => other,
        (Some(prev), Some(next)) => {
            let merged = deep_merge_source_value(
                prev.source_projection().clone(),
                next.source_projection().clone(),
            )
            .expect("valid skill override records have no array-length coercion boundary");
            Some(
                crate::settings::schema::SkillOverrides::from_source_projection(merged)
                    .expect("merging valid skill override records keeps valid enum values"),
            )
        }
    }
}

fn merge_model_settings(
    prev: Option<indexmap::IndexMap<String, crate::settings::schema::ModelSettings>>,
    next: Option<indexmap::IndexMap<String, crate::settings::schema::ModelSettings>>,
) -> Option<indexmap::IndexMap<String, crate::settings::schema::ModelSettings>> {
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
    prev: Option<crate::settings::schema::FusionRegistrySettingsJson>,
    next: Option<crate::settings::schema::FusionRegistrySettingsJson>,
) -> Option<crate::settings::schema::FusionRegistrySettingsJson> {
    match (prev, next) {
        (None, None) => None,
        (Some(v), None) | (None, Some(v)) => Some(v),
        (Some(p), Some(n)) => Some(crate::settings::schema::FusionRegistrySettingsJson {
            enabled: n.enabled.or(p.enabled),
            proactive: n.proactive.or(p.proactive),
            allowed_profiles: n.allowed_profiles.or(p.allowed_profiles),
            allow_cross_provider_for_agent: n
                .allow_cross_provider_for_agent
                .or(p.allow_cross_provider_for_agent),
            slash_cross_provider_default: n
                .slash_cross_provider_default
                .or(p.slash_cross_provider_default),
            // The merged view is informational only. Selection consumes the
            // retained source layers, so records and defaults never change scope.
            strategies: n.strategies,
            default_strategy_id: n.default_strategy_id,
            overrides: n.overrides.or(p.overrides),
            diagnostic: n.diagnostic.or(p.diagnostic),
        }),
    }
}

/// Dense serde-value interoperability fold. This adapter cannot preserve an
/// array hole across multiple layers because `serde_json::Value` has no
/// `undefined` value. Source-preserving settings stacks must use
/// [`merge_raw_source_layer`] and keep its projection until their final JSON
/// output boundary.
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
/// Strategy choice comes from [`crate::settings::schema::strategy_for`]. The
/// projection method above is the authoritative Native-source path whenever
/// a later merge can observe holes. This helper exists only for explicitly
/// requested dense JSON consumers.
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
pub fn merge_raw_layer(
    prev: &mut std::collections::BTreeMap<String, serde_json::Value>,
    next: std::collections::BTreeMap<String, serde_json::Value>,
) -> Result<Vec<String>, Utf16JsonProjectionError> {
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
        let merged = if matches!(
            key.as_str(),
            "modelPicker" | "extraKnownMarketplaces" | "managedMcpServers"
        ) {
            merge_native_settings_value(v_prev, v_next.clone(), &key)?
        } else {
            match strategy_for(&key).unwrap_or(MergeStrategy::Override) {
                MergeStrategy::Override => v_next.clone(),
                MergeStrategy::ConcatDedup => match (&v_prev, &v_next) {
                    (Value::Array(_), Value::Array(_)) => {
                        merge_native_settings_value(v_prev.clone(), v_next.clone(), &key)?
                    }
                    // A non-array on either side cannot be concatenated, and the
                    // typed merge would not even have parsed it. `next` wins, as it
                    // does for every shape mismatch in `deep_merge_value`.
                    (_, n) => n.clone(),
                },
                // Non-objects fall through `deep_merge_value`'s own mismatch arm to
                // `next`, matching `deep_merge_object` / `deep_merge_value_opt`.
                MergeStrategy::DeepMerge if key == "fusion" => match (v_prev, v_next.clone()) {
                    (Value::Object(mut previous), Value::Object(next)) => {
                        previous.remove("strategies");
                        previous.remove("defaultStrategyId");
                        previous.extend(next);
                        previous
                            .entry("strategies")
                            .or_insert_with(|| Value::Array(Vec::new()));
                        Value::Object(previous)
                    }
                    (_, next) => next,
                },
                MergeStrategy::DeepMerge => {
                    merge_native_settings_value(v_prev, v_next.clone(), &key)?
                }
            }
        };
        if merged != v_next {
            unioned.push(key.clone());
        }
        prev.insert(key, merged);
    }
    Ok(unioned)
}

/// Preserve Fusion global inheritance without merging strategy registries.
/// Source strings and keys retain their original UTF-16 units.
fn merge_fusion_source_value(
    previous: &Utf16JsonProjection,
    next: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
    let (Some(previous_entries), Some(next_entries)) =
        (previous.object_entries(), next.object_entries())
    else {
        return Ok(next.clone());
    };
    let mut fields = Vec::new();
    for entry in previous_entries {
        if matches!(entry.display_key, "strategies" | "defaultStrategyId") {
            continue;
        }
        fields.push((
            entry.code_units.to_vec(),
            previous.subprojection_property_units(entry.code_units)?,
        ));
    }
    for entry in next_entries {
        let value = next.subprojection_property_units(entry.code_units)?;
        if let Some((_, existing)) = fields
            .iter_mut()
            .find(|(key, _)| key.as_slice() == entry.code_units)
        {
            *existing = value;
        } else {
            fields.push((entry.code_units.to_vec(), value));
        }
    }
    let strategy_key = "strategies".encode_utf16().collect::<Vec<_>>();
    if !fields.iter().any(|(key, _)| key == &strategy_key) {
        fields.push((strategy_key, Utf16JsonProjection::parse("[]")?));
    }
    Utf16JsonProjection::object(fields)
}

/// Fold one exact source settings layer into another using the same per-key
/// strategy table as [`merge_raw_layer`]. Values and keys stay in Core's flat
/// UTF-16 projection, including unpaired surrogates; this is the source view
/// used by settings snapshots and editors.
pub fn merge_raw_source_layer(
    prev: &mut Utf16JsonProjection,
    next: &Utf16JsonProjection,
) -> Result<Vec<String>, Utf16JsonProjectionError> {
    let previous_entries = prev
        .object_entries()
        .ok_or(Utf16JsonProjectionError::ExpectedObject)?;
    let next_entries = next
        .object_entries()
        .ok_or(Utf16JsonProjectionError::ExpectedObject)?;
    let mut fields = Vec::with_capacity(previous_entries.len() + next_entries.len());
    for entry in previous_entries {
        fields.push((
            entry.code_units.to_vec(),
            prev.subprojection_property_units(entry.code_units)?,
        ));
    }

    let mut unioned = Vec::new();
    for entry in next_entries {
        if entry.value.is_null() {
            continue;
        }
        let next_value = next.subprojection_property_units(entry.code_units)?;
        let Some(index) = fields
            .iter()
            .position(|(key_units, _)| key_units.as_slice() == entry.code_units)
        else {
            fields.push((entry.code_units.to_vec(), next_value));
            continue;
        };

        let previous_value = &fields[index].1;
        let merged = if matches!(
            entry.display_key,
            "modelPicker" | "extraKnownMarketplaces" | "managedMcpServers"
        ) {
            merge_native_settings_source_value(
                previous_value.clone(),
                &next_value,
                entry.display_key,
            )?
        } else {
            match strategy_for(entry.display_key).unwrap_or(MergeStrategy::Override) {
                MergeStrategy::Override => next_value.clone(),
                MergeStrategy::ConcatDedup => concat_dedup_source(previous_value, &next_value)?,
                MergeStrategy::DeepMerge if entry.display_key == "fusion" => {
                    merge_fusion_source_value(previous_value, &next_value)?
                }
                MergeStrategy::DeepMerge if entry.display_key == "permissions" => {
                    merge_source_value(previous_value, &next_value, true)?
                }
                MergeStrategy::DeepMerge if entry.display_key == "policyHelpers" => {
                    merge_native_settings_source_value(
                        previous_value.clone(),
                        &next_value,
                        "policyHelpers",
                    )?
                }
                MergeStrategy::DeepMerge => merge_source_value(previous_value, &next_value, false)?,
            }
        };
        if !source_values_equal(&merged, &next_value) {
            unioned.push(entry.display_key.to_owned());
        }
        fields[index].1 = merged;
    }

    *prev = Utf16JsonProjection::object_units(fields)?;
    Ok(unioned)
}

fn merge_source_value(
    previous: &Utf16JsonProjection,
    next: &Utf16JsonProjection,
    _permissions: bool,
) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
    deep_merge_source_value(previous.clone(), next.clone())
}

fn concat_dedup_source(
    previous: &Utf16JsonProjection,
    next: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
    if previous.array_len("").is_none() || next.array_len("").is_none() {
        return Ok(next.clone());
    }
    merge_native_settings_arrays_source(
        previous,
        previous.array_len("").expect("validated previous array") as u32,
        next,
        next.array_len("").expect("validated next array") as u32,
    )
}

fn source_values_equal(left: &Utf16JsonProjection, right: &Utf16JsonProjection) -> bool {
    let (Some(left_root), Some(right_root)) = (left.at_pointer(""), right.at_pointer("")) else {
        return false;
    };
    if left_root.is_undefined() || right_root.is_undefined() {
        return left_root.is_undefined() && right_root.is_undefined();
    }
    let mut pending = vec![(left_root, right_root)];
    while let Some((left, right)) = pending.pop() {
        if left.is_null() || right.is_null() {
            if left.is_null() && right.is_null() {
                continue;
            }
            return false;
        }
        match (left.as_bool(), right.as_bool()) {
            (Some(left), Some(right)) => {
                if left != right {
                    return false;
                }
                continue;
            }
            (Some(_), None) | (None, Some(_)) => return false,
            (None, None) => {}
        }
        match (left.as_number(), right.as_number()) {
            (Some(left), Some(right)) => {
                if left != right && !(left.is_nan() && right.is_nan()) {
                    return false;
                }
                continue;
            }
            (Some(_), None) | (None, Some(_)) => return false,
            (None, None) => {}
        }
        match (left.code_units(), right.code_units()) {
            (Some(left), Some(right)) => {
                if left != right {
                    return false;
                }
                continue;
            }
            (Some(_), None) | (None, Some(_)) => return false,
            (None, None) => {}
        }
        match (left.array_entries(), right.array_entries()) {
            (Some((left_length, left_entries)), Some((right_length, right_entries))) => {
                if left_length != right_length
                    || left_entries.len() != right_entries.len()
                    || left_entries
                        .iter()
                        .zip(&right_entries)
                        .any(|((left_index, _), (right_index, _))| left_index != right_index)
                {
                    return false;
                }
                for ((_, left), (_, right)) in left_entries.into_iter().zip(right_entries) {
                    pending.push((left, right));
                }
                continue;
            }
            (Some(_), None) | (None, Some(_)) => return false,
            (None, None) => {}
        }
        match (left.object_entries(), right.object_entries()) {
            (Some(left_entries), Some(right_entries)) => {
                if left_entries.len() != right_entries.len() {
                    return false;
                }
                for left_entry in left_entries {
                    let Some(right_entry) = right_entries
                        .iter()
                        .find(|entry| entry.code_units == left_entry.code_units)
                    else {
                        return false;
                    };
                    pending.push((left_entry.value, right_entry.value));
                }
            }
            _ => return false,
        }
    }
    true
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

/// Native's `N3` shallow-spreads these two plain-object settings fields.
/// Child objects at a colliding marketplace/server key are replaced whole.
fn shallow_merge_object(
    prev: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    next: Option<std::collections::BTreeMap<String, serde_json::Value>>,
) -> Option<std::collections::BTreeMap<String, serde_json::Value>> {
    match (prev, next) {
        (None, other) | (other, None) => other,
        (Some(mut prev), Some(next)) => {
            prev.extend(next);
            Some(prev)
        }
    }
}

/// Deep-merge plugin configuration JSON without converting it through
/// `serde_json::Value`. Existing keys retain their earlier position, new keys
/// append in the later source order, and values/keys keep their exact source
/// strings and JavaScript number representation. Non-object roots follow the
/// same mismatch behavior as `deep_merge_value`: the later value wins.
fn deep_merge_source_object(
    prev: Option<Utf16JsonProjection>,
    next: Option<Utf16JsonProjection>,
) -> Result<Option<Utf16JsonProjection>, Utf16JsonProjectionError> {
    match (prev, next) {
        (None, other) | (other, None) => Ok(other),
        (Some(prev), Some(next)) => Ok(Some(deep_merge_source_value(prev, next)?)),
    }
}

fn merge_policy_helpers_source_object(
    prev: Option<Utf16JsonProjection>,
    next: Option<Utf16JsonProjection>,
) -> Result<Option<Utf16JsonProjection>, Utf16JsonProjectionError> {
    match (prev, next) {
        (None, other) | (other, None) => Ok(other),
        (Some(prev), Some(next)) => Ok(Some(merge_native_settings_source_value(
            prev,
            &next,
            "policyHelpers",
        )?)),
    }
}

/// Native's `N3` customizer applies array union, `fallbackModel` replacement,
/// `modelPicker` replacement, and shallow object replacement for the two
/// marketplace/MCP maps. Values not handled by that customizer follow Native
/// `Nf` merge behavior. The UTF-16 implementation also models source-object
/// numeric indices merged into an existing target array.
fn merge_native_settings_source_value(
    prev: Utf16JsonProjection,
    next: &Utf16JsonProjection,
    root_property_key: &str,
) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
    const FALLBACK_MODEL_KEY: &[u16] = &[
        0x0066, 0x0061, 0x006c, 0x006c, 0x0062, 0x0061, 0x0063, 0x006b, 0x004d, 0x006f, 0x0064,
        0x0065, 0x006c,
    ];

    #[derive(Clone)]
    enum SparseArrayBuildEntry {
        Index(u32),
        Property(Vec<u16>),
    }

    enum SparseArrayOperation {
        Merge {
            entry: SparseArrayBuildEntry,
            previous: Option<Utf16JsonProjection>,
            next: Option<Utf16JsonProjection>,
            key: Vec<u16>,
        },
        SetLength(Utf16JsonProjection),
    }

    enum Task {
        Merge(
            Option<Utf16JsonProjection>,
            Option<Utf16JsonProjection>,
            Option<Vec<u16>>,
        ),
        BuildObject {
            keys: Vec<Vec<u16>>,
            child_count: usize,
            child_positions: Vec<usize>,
        },
        BuildSparseArray {
            pre_length: u32,
            has_explicit_length: bool,
            entries: Vec<SparseArrayBuildEntry>,
            property_order: Vec<Vec<u16>>,
            child_count: usize,
        },
        SetSparseArrayLength(Utf16JsonProjection),
    }

    let mut tasks = vec![Task::Merge(
        Some(prev),
        Some(next.clone()),
        Some(root_property_key.encode_utf16().collect()),
    )];
    let mut values = Vec::<Utf16JsonProjection>::new();
    let mut validated_array_lengths = Vec::<u32>::new();

    while let Some(task) = tasks.pop() {
        match task {
            Task::Merge(None, Some(value), _) | Task::Merge(Some(value), None, _) => {
                values.push(value);
            }
            Task::Merge(None, None, _) => {
                return Err(Utf16JsonProjectionError::InvalidProjection(
                    "Native merge task has neither a previous nor next value",
                ));
            }
            Task::Merge(Some(prev), Some(next), property_key) => {
                let is_model_picker = source_property_key_is(&property_key, "modelPicker");
                if is_model_picker {
                    // N3 calls fc(next), whose object/options copies preserve
                    // this JSON projection's exact observable value. It does
                    // not recursively merge a previous modelPicker.
                    values.push(next);
                    continue;
                }

                if let (Some(prev_entries), Some(next_entries)) =
                    (prev.object_entries(), next.object_entries())
                {
                    let is_shallow_native_object =
                        source_property_key_is(&property_key, "extraKnownMarketplaces")
                            || source_property_key_is(&property_key, "managedMcpServers");
                    if is_shallow_native_object {
                        values.push(shallow_merge_source_objects(&prev, &next)?);
                        continue;
                    }

                    let prev_keys = prev_entries
                        .iter()
                        .map(|entry| entry.code_units.to_vec())
                        .collect::<Vec<_>>();
                    let mut keys = prev_keys;
                    let mut output_positions = keys
                        .iter()
                        .enumerate()
                        .map(|(index, key)| (key.clone(), index))
                        .collect::<std::collections::HashMap<_, _>>();
                    let previous_positions = output_positions.clone();
                    let mut children = Vec::with_capacity(prev_entries.len() + next_entries.len());
                    let mut touched = std::collections::HashSet::new();

                    // Native Nf visits only source properties, in JavaScript
                    // Object.keys order. Keep that execution order for nested
                    // ArraySetLength failures while separately preserving the
                    // target object's insertion order in the result.
                    for key in crate::host::mcp_result::javascript_object_keys(&next) {
                        let next_child = next
                            .subprojection_property_units(&key)
                            .expect("enumerated later policy helper property exists");
                        let is_proto = key.iter().copied().eq("__proto__".encode_utf16());
                        let next_is_composite = next_child.object_entries().is_some()
                            || next_child.array_entries().is_some();
                        if is_proto && next_is_composite {
                            // SafeGet returns the existing own composite value;
                            // recursive assignment to the prototype setter is
                            // ignored. A new composite __proto__ is not copied.
                            continue;
                        }
                        let position = match output_positions.get(&key).copied() {
                            Some(position) => position,
                            None => {
                                let position = keys.len();
                                keys.push(key.clone());
                                output_positions.insert(key.clone(), position);
                                position
                            }
                        };
                        let previous = if is_proto || !previous_positions.contains_key(&key) {
                            None
                        } else {
                            Some(prev.subprojection_property_units(&key)?)
                        };
                        touched.insert(key.clone());
                        children.push((position, previous, Some(next_child), Some(key)));
                    }

                    // Untouched target properties retain their original value.
                    // Schedule them after source keys; these identity steps do
                    // not change the first Native error from a source property.
                    for (position, key) in keys.iter().enumerate().take(prev_entries.len()) {
                        if touched.contains(key) {
                            continue;
                        }
                        let previous = prev
                            .subprojection_property_units(key)
                            .expect("enumerated previous policy helper property exists");
                        children.push((position, Some(previous), None, Some(key.clone())));
                    }

                    let child_count = children.len();
                    let child_positions = children.iter().map(|(position, ..)| *position).collect();
                    tasks.push(Task::BuildObject {
                        keys,
                        child_count,
                        child_positions,
                    });
                    tasks.extend(
                        children
                            .into_iter()
                            .rev()
                            .map(|(_, prev, next, key)| Task::Merge(prev, next, key)),
                    );
                    continue;
                }

                let previous_array_len = prev
                    .at_pointer("")
                    .and_then(JsonValueView::array_len)
                    .and_then(|length| u32::try_from(length).ok());
                let next_array_len = next
                    .at_pointer("")
                    .and_then(JsonValueView::array_len)
                    .and_then(|length| u32::try_from(length).ok());
                if let (Some(previous_length), Some(next_length)) =
                    (previous_array_len, next_array_len)
                {
                    if property_key.as_deref() == Some(FALLBACK_MODEL_KEY) {
                        values.push(next);
                    } else {
                        values.push(merge_native_settings_arrays_source(
                            &prev,
                            previous_length,
                            &next,
                            next_length,
                        )?);
                    }
                } else if let (Some(previous_length), Some(_)) =
                    (previous_array_len, next.object_entries())
                {
                    // Native Nf mutates the existing array with enumerable
                    // object keys. Keep non-index own properties in the flat
                    // state even though JSON.stringify omits them. Source
                    // operations follow JavaScript Object.keys order so a
                    // nested merge error before/after `length` remains ordered.
                    let previous_index_values = prev
                        .array_entries()
                        .expect("previous array has array entries")
                        .1
                        .into_iter()
                        .map(|(index, _)| {
                            (
                                index,
                                prev.subprojection(&format!("/{index}"))
                                    .expect("present previous array entry exists"),
                            )
                        })
                        .collect::<std::collections::BTreeMap<_, _>>();
                    let previous_named_views = prev
                        .array_named_properties()
                        .expect("previous array has named-property view");
                    let mut previous_named_values = std::collections::HashMap::new();
                    let mut property_order = Vec::with_capacity(previous_named_views.len());
                    for entry in &previous_named_views {
                        let key = entry.code_units.to_vec();
                        property_order.push(key.clone());
                        previous_named_values.insert(
                            key.clone(),
                            prev.subprojection_array_property_units(&key)
                                .expect("enumerated previous array property exists"),
                        );
                    }

                    let mut operations = Vec::<SparseArrayOperation>::new();
                    let mut merged_indices = std::collections::HashSet::new();
                    let mut merged_properties = std::collections::HashSet::new();
                    let mut pre_length = previous_length;
                    let mut has_explicit_length = false;

                    for key in crate::host::mcp_result::javascript_object_keys(&next) {
                        if key.iter().copied().eq("length".encode_utf16()) {
                            has_explicit_length = true;
                            operations.push(SparseArrayOperation::SetLength(
                                next.subprojection_property_units(&key)
                                    .expect("enumerated source length property exists"),
                            ));
                            continue;
                        }
                        let next_child = next
                            .subprojection_property_units(&key)
                            .expect("enumerated source object property exists");
                        if let Some(index) = parse_array_index(&key) {
                            pre_length = pre_length.max(index.saturating_add(1));
                            let previous = previous_index_values.get(&index).cloned();
                            merged_indices.insert(index);
                            operations.push(SparseArrayOperation::Merge {
                                entry: SparseArrayBuildEntry::Index(index),
                                previous,
                                next: Some(next_child),
                                key,
                            });
                            continue;
                        }

                        let previous = previous_named_values.get(&key).cloned();
                        let is_proto = key.iter().copied().eq("__proto__".encode_utf16());
                        let next_is_composite = next_child.object_entries().is_some()
                            || next_child.array_entries().is_some();
                        if is_proto && next_is_composite {
                            // Native's safe getter returns undefined for
                            // `__proto__` on both sides for object-valued
                            // recursion, so that branch writes undefined and
                            // the setter leaves an existing property alone.
                            continue;
                        }
                        let previous = if is_proto { None } else { previous };
                        if !previous_named_values.contains_key(&key) {
                            property_order.push(key.clone());
                        }
                        merged_properties.insert(key.clone());
                        operations.push(SparseArrayOperation::Merge {
                            entry: SparseArrayBuildEntry::Property(key.clone()),
                            previous,
                            next: Some(next_child),
                            key,
                        });
                    }

                    // Preserve existing own entries not touched by this source.
                    // These identity operations cannot fail and therefore run
                    // after source keys without affecting Native error order.
                    for (index, previous) in previous_index_values {
                        if merged_indices.insert(index) {
                            operations.push(SparseArrayOperation::Merge {
                                entry: SparseArrayBuildEntry::Index(index),
                                previous: Some(previous),
                                next: None,
                                key: index.to_string().encode_utf16().collect(),
                            });
                        }
                    }
                    for (key, previous) in previous_named_values {
                        if merged_properties.insert(key.clone()) {
                            operations.push(SparseArrayOperation::Merge {
                                entry: SparseArrayBuildEntry::Property(key.clone()),
                                previous: Some(previous),
                                next: None,
                                key,
                            });
                        }
                    }

                    let mut build_entries = Vec::with_capacity(operations.len());
                    let mut child_count = 0;
                    for operation in &operations {
                        if let SparseArrayOperation::Merge { entry, .. } = operation {
                            build_entries.push(entry.clone());
                            child_count += 1;
                        }
                    }
                    tasks.push(Task::BuildSparseArray {
                        pre_length,
                        has_explicit_length,
                        entries: build_entries,
                        property_order,
                        child_count,
                    });
                    tasks.extend(
                        operations
                            .into_iter()
                            .rev()
                            .map(|operation| match operation {
                                SparseArrayOperation::Merge {
                                    previous,
                                    next,
                                    key,
                                    ..
                                } => Task::Merge(previous, next, Some(key)),
                                SparseArrayOperation::SetLength(value) => {
                                    Task::SetSparseArrayLength(value)
                                }
                            }),
                    );
                    continue;
                } else if prev.object_entries().is_some() && next_array_len.is_some() {
                    // A source array replaces an existing plain-object target
                    // with a fresh array in the Native Nf merge path.
                    values.push(next);
                    continue;
                } else {
                    values.push(next);
                }
            }
            Task::BuildObject {
                keys,
                child_count,
                child_positions,
            } => {
                let children = values.split_off(values.len() - child_count);
                if children.len() != child_positions.len() {
                    return Err(Utf16JsonProjectionError::InvalidProjection(
                        "Native object merge child mapping is inconsistent",
                    ));
                }
                let mut output_values = (0..keys.len()).map(|_| None).collect::<Vec<_>>();
                for (position, value) in child_positions.into_iter().zip(children) {
                    let slot = output_values.get_mut(position).ok_or(
                        Utf16JsonProjectionError::InvalidProjection(
                            "Native object merge child position is outside the object",
                        ),
                    )?;
                    *slot = Some(value);
                }
                let fields = keys
                    .into_iter()
                    .zip(output_values)
                    .map(|(key, value)| {
                        value.map(|value| (key, value)).ok_or(
                            Utf16JsonProjectionError::InvalidProjection(
                                "Native object merge left a property uninitialized",
                            ),
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let merged = Utf16JsonProjection::object_units(fields)?;
                values.push(merged);
            }
            Task::BuildSparseArray {
                pre_length,
                has_explicit_length,
                entries,
                property_order,
                child_count,
            } => {
                let children = values.split_off(values.len() - child_count);
                let final_length = if has_explicit_length {
                    validated_array_lengths.pop().ok_or(
                        Utf16JsonProjectionError::InvalidProjection(
                            "Native array length task did not produce a value",
                        ),
                    )?
                } else {
                    pre_length
                };
                let mut indexed = std::collections::BTreeMap::new();
                let mut named = std::collections::HashMap::new();
                for (entry, child) in entries.into_iter().zip(children) {
                    match entry {
                        SparseArrayBuildEntry::Index(index) => {
                            indexed.insert(index, child);
                        }
                        SparseArrayBuildEntry::Property(key) => {
                            named.insert(key, child);
                        }
                    }
                }
                let indexed = indexed
                    .into_iter()
                    .filter(|(index, _)| *index < final_length)
                    .collect();
                let properties = property_order
                    .into_iter()
                    .filter_map(|key| named.remove(&key).map(|value| (key, value)))
                    .collect();
                values.push(Utf16JsonProjection::sparse_array_with_properties(
                    final_length,
                    indexed,
                    properties,
                )?);
            }
            Task::SetSparseArrayLength(value) => {
                validated_array_lengths.push(source_array_length_value(&value)?);
            }
        }
    }

    Ok(values
        .pop()
        .expect("Native settings source merge produces one root value"))
}

fn shallow_merge_source_objects(
    previous: &Utf16JsonProjection,
    next: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
    let previous_entries = previous
        .object_entries()
        .ok_or(Utf16JsonProjectionError::ExpectedObject)?;
    let next_entries = next
        .object_entries()
        .ok_or(Utf16JsonProjectionError::ExpectedObject)?;
    let mut fields = previous_entries
        .iter()
        .map(|entry| {
            Ok((
                entry.code_units.to_vec(),
                previous.subprojection_property_units(entry.code_units)?,
            ))
        })
        .collect::<Result<Vec<_>, Utf16JsonProjectionError>>()?;
    for entry in next_entries {
        let value = next.subprojection_property_units(entry.code_units)?;
        if let Some((_, previous_value)) = fields
            .iter_mut()
            .find(|(key, _)| key.as_slice() == entry.code_units)
        {
            *previous_value = value;
        } else {
            fields.push((entry.code_units.to_vec(), value));
        }
    }
    Utf16JsonProjection::object_units(fields)
}

fn source_property_key_is(property_key: &Option<Vec<u16>>, expected: &str) -> bool {
    property_key
        .as_deref()
        .is_some_and(|key| key.iter().copied().eq(expected.encode_utf16()))
}

fn parse_array_index(units: &[u16]) -> Option<u32> {
    if units.is_empty() || (units.len() > 1 && units[0] == u16::from(b'0')) {
        return None;
    }
    let mut value = 0u64;
    for unit in units {
        if !(u16::from(b'0')..=u16::from(b'9')).contains(unit) {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(u64::from(*unit - u16::from(b'0')))?;
    }
    // ECMAScript array indices stop at 2^32-2; 2^32-1 is an ordinary key.
    if value >= u64::from(u32::MAX) {
        return None;
    }
    u32::try_from(value).ok()
}

/// JavaScript `ToNumber` for JSON values admitted at this settings merge
/// boundary. Arrays use their default `join` primitive without materializing a
/// potentially sparse logical array: a join with two or more items contains a
/// comma and therefore cannot be a numeric string; a zero- or one-item join
/// can be reduced recursively.
fn source_to_number(
    value: JsonValueView<'_>,
    raw_value: &Utf16JsonProjection,
) -> Result<f64, Utf16JsonProjectionError> {
    if value.is_undefined() {
        return Ok(f64::NAN);
    }
    if value.is_null() {
        return Ok(0.0);
    }
    if let Some(number) = value.as_number() {
        return Ok(number);
    }
    if let Some(boolean) = value.as_bool() {
        return Ok(if boolean { 1.0 } else { 0.0 });
    }
    if let Some(units) = value.code_units() {
        return Ok(number_from_js_string_units(units));
    }
    if value.object_entries().is_some() {
        if value.get_property("toString").is_some() {
            return Err(Utf16JsonProjectionError::InvalidArrayLengthPrimitive {
                raw_value: Box::new(raw_value.clone()),
            });
        }
        return Ok(f64::NAN);
    }
    if value.array_len().is_some() {
        return array_to_number(value, raw_value);
    }
    Ok(f64::NAN)
}

fn array_to_number(
    value: JsonValueView<'_>,
    raw_value: &Utf16JsonProjection,
) -> Result<f64, Utf16JsonProjectionError> {
    match value.array_len() {
        Some(0) => Ok(0.0),
        Some(1) => value
            .get_index(0)
            .map(|item| array_item_to_number(item, raw_value))
            // Array#join emits an empty field for a one-element hole, so
            // Number("") is +0 rather than NaN.
            .unwrap_or(Ok(0.0)),
        // Array#join inserts a comma between every pair of items. A comma is
        // not part of the ECMAScript StringNumericLiteral grammar.
        Some(_) | None => Ok(f64::NAN),
    }
}

fn array_item_to_number(
    value: JsonValueView<'_>,
    raw_value: &Utf16JsonProjection,
) -> Result<f64, Utf16JsonProjectionError> {
    if value.is_undefined() || value.is_null() {
        // join converts both null and undefined items to empty strings, and
        // Number("") is +0.
        return Ok(0.0);
    }
    if let Some(units) = value.code_units() {
        return Ok(number_from_js_string_units(units));
    }
    if let Some(number) = value.as_number() {
        let spelling = js_number_to_string(number);
        return Ok(number_from_js_string_units(
            &spelling.encode_utf16().collect::<Vec<_>>(),
        ));
    }
    if value.array_len().is_some() {
        return array_to_number(value, raw_value);
    }
    if value.object_entries().is_some() && value.get_property("toString").is_some() {
        return Err(Utf16JsonProjectionError::InvalidArrayLengthPrimitive {
            raw_value: Box::new(raw_value.clone()),
        });
    }
    // Boolean and object array items stringify to "true"/"false" and
    // "[object Object]", neither of which is numeric.
    Ok(f64::NAN)
}

fn js_number_to_string(number: f64) -> String {
    if number.is_nan() {
        return "NaN".to_owned();
    }
    if number == f64::INFINITY {
        return "Infinity".to_owned();
    }
    if number == f64::NEG_INFINITY {
        return "-Infinity".to_owned();
    }
    if number == 0.0 {
        return "0".to_owned();
    }
    let mut buffer = ryu_js::Buffer::new();
    buffer.format_finite(number).to_owned()
}

fn number_from_js_string_units(units: &[u16]) -> f64 {
    let Ok(text) = String::from_utf16(units) else {
        return f64::NAN;
    };
    let text = text.trim_matches(is_js_numeric_whitespace);
    if text.is_empty() {
        return 0.0;
    }
    match text {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    if text.len() > 2 && !text.starts_with('+') && !text.starts_with('-') {
        let (radix, digits) = match text.get(..2) {
            Some(prefix) if prefix.eq_ignore_ascii_case("0x") => (16, &text[2..]),
            Some(prefix) if prefix.eq_ignore_ascii_case("0b") => (2, &text[2..]),
            Some(prefix) if prefix.eq_ignore_ascii_case("0o") => (8, &text[2..]),
            _ => (0, ""),
        };
        if radix != 0 {
            if digits.is_empty() {
                return f64::NAN;
            }
            let mut value = 0.0;
            for ch in digits.chars() {
                let Some(digit) = ch.to_digit(radix) else {
                    return f64::NAN;
                };
                value = value * f64::from(radix) + f64::from(digit);
            }
            return value;
        }
    }
    if !valid_js_decimal_number(text) {
        return f64::NAN;
    }
    text.parse::<f64>().unwrap_or(f64::NAN)
}

fn is_js_numeric_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\u{0009}'..='\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn valid_js_decimal_number(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let integer_start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    let mut digits = index - integer_start;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        digits += index - fraction_start;
    }
    if digits == 0 {
        return false;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == exponent_start {
            return false;
        }
    }
    index == bytes.len()
}

/// Native's array-target merge assigns a source `length` after enumerable
/// index properties. Apply JavaScript ToNumber to the exact source value, then
/// let ArraySetLength's valid integer range decide whether it can be assigned.
/// Invalid input returns the same RangeError class and retains the raw value.
fn source_array_length_value(value: &Utf16JsonProjection) -> Result<u32, Utf16JsonProjectionError> {
    let view = value
        .at_pointer("")
        .ok_or(Utf16JsonProjectionError::InvalidPointer)?;
    let number = source_to_number(view, value)?;
    if !number.is_finite()
        || number.fract() != 0.0
        || !(0.0..=f64::from(u32::MAX)).contains(&number)
    {
        return Err(Utf16JsonProjectionError::InvalidArraySetLength {
            raw_value: Box::new(value.clone()),
            coerced_number: (!number.is_nan()).then_some(number),
        });
    }
    Ok(number as u32)
}

fn merge_native_settings_arrays_source(
    previous: &Utf16JsonProjection,
    _previous_length: u32,
    next: &Utf16JsonProjection,
    _next_length: u32,
) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
    let previous_candidates = source_array_set_candidates(previous)?;
    let next_candidates = source_array_set_candidates(next)?;
    let mut seen = Vec::<JsonValueView<'_>>::new();
    let mut output_entries = Vec::<(u32, Utf16JsonProjection)>::new();
    let mut output_length = 0_u32;

    for (source, candidates) in [(previous, previous_candidates), (next, next_candidates)] {
        for (index, value) in candidates {
            if seen
                .iter()
                .copied()
                .any(|existing| source_same_value_zero(existing, value))
            {
                continue;
            }
            seen.push(value);
            if let Some(index) = index {
                output_entries.push((output_length, source.subprojection(&format!("/{index}"))?));
            }
            output_length =
                output_length
                    .checked_add(1)
                    .ok_or(Utf16JsonProjectionError::InvalidProjection(
                        "Native array union exceeds the JavaScript array length range",
                    ))?;
        }
    }

    Utf16JsonProjection::sparse_array(output_length, output_entries)
}

/// Produce the values spread into Native's `Set([...previous, ...next])`.
/// A sparse input contributes at most one implicit `undefined` candidate,
/// placed where its first hole occurs; later holes are duplicate Set values.
fn source_array_set_candidates<'a>(
    source: &'a Utf16JsonProjection,
) -> Result<Vec<(Option<u32>, JsonValueView<'a>)>, Utf16JsonProjectionError> {
    let (length, entries) = source
        .array_entries()
        .ok_or(Utf16JsonProjectionError::InvalidPointer)?;
    let mut result = Vec::with_capacity(entries.len().saturating_add(1));
    let mut expected_index = 0_u32;
    let mut undefined_added = false;
    for (index, value) in entries {
        if !undefined_added && expected_index < index {
            result.push((
                None,
                source
                    .at_pointer(&format!("/{expected_index}"))
                    .ok_or(Utf16JsonProjectionError::InvalidPointer)?,
            ));
            undefined_added = true;
        }
        result.push((Some(index), value));
        expected_index = index.saturating_add(1);
    }
    if !undefined_added && expected_index < length {
        result.push((
            None,
            source
                .at_pointer(&format!("/{expected_index}"))
                .ok_or(Utf16JsonProjectionError::InvalidPointer)?,
        ));
    }
    Ok(result)
}

fn source_same_value_zero(left: JsonValueView<'_>, right: JsonValueView<'_>) -> bool {
    if left.is_undefined() || right.is_undefined() {
        return left.is_undefined() && right.is_undefined();
    }
    if left.is_null() || right.is_null() {
        return left.is_null() && right.is_null();
    }
    if let (Some(left), Some(right)) = (left.as_bool(), right.as_bool()) {
        return left == right;
    }
    if let (Some(left), Some(right)) = (left.as_number(), right.as_number()) {
        return left == right || (left.is_nan() && right.is_nan());
    }
    if let (Some(left), Some(right)) = (left.code_units(), right.code_units()) {
        return left == right;
    }
    // JavaScript Set compares objects by identity, not structure. Distinct
    // entries in parsed source trees are distinct objects.
    false
}

fn deep_merge_source_value(
    prev: Utf16JsonProjection,
    next: Utf16JsonProjection,
) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
    // N3 is the same customizer used by the generic Native settings merge:
    // arrays union, fallbackModel replaces, named shallow objects spread,
    // while ordinary object keys recurse through Nf.
    merge_native_settings_source_value(prev, &next, "")
}

fn merge_permissions_object(
    prev: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    next: Option<std::collections::BTreeMap<String, serde_json::Value>>,
) -> Option<std::collections::BTreeMap<String, serde_json::Value>> {
    match (prev, next) {
        (None, other) | (other, None) => other,
        (Some(prev), Some(next)) => {
            let merged = merge_permissions_value(
                serde_json::Value::Object(prev.into_iter().collect()),
                serde_json::Value::Object(next.into_iter().collect()),
            );
            match merged {
                serde_json::Value::Object(merged) => Some(merged.into_iter().collect()),
                _ => unreachable!("two permission objects merge to an object"),
            }
        }
    }
}

fn merge_permissions_value(prev: serde_json::Value, next: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match (prev, next) {
        (Value::Object(mut prev), Value::Object(next)) => {
            for (key, value) in next {
                let merged = match prev.remove(&key) {
                    Some(earlier) => merge_permissions_value(earlier, value),
                    None => value,
                };
                prev.insert(key, merged);
            }
            Value::Object(prev)
        }
        (Value::Array(prev), Value::Array(next)) => {
            Value::Array(concat_dedup(Some(prev), Some(next)).unwrap_or_default())
        }
        (_, next) => next,
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

fn merge_native_settings_value(
    prev: serde_json::Value,
    next: serde_json::Value,
    property_key: &str,
) -> Result<serde_json::Value, Utf16JsonProjectionError> {
    use serde_json::Value;

    if property_key == "modelPicker" {
        // `fc(next)` copies the picker and options records without merging the
        // previous picker; its JSON-visible projection is exactly `next`.
        return Ok(next);
    }

    if matches!(property_key, "extraKnownMarketplaces" | "managedMcpServers") {
        if let (Value::Object(previous), Value::Object(next_object)) = (&prev, &next) {
            let mut previous = previous.clone();
            previous.extend(next_object.clone());
            return Ok(Value::Object(previous));
        }
    }

    match (prev, next) {
        (Value::Object(mut prev), Value::Object(next)) => {
            for (key, value) in next {
                let merged = match prev.remove(&key) {
                    Some(previous) => merge_native_settings_value(previous, value, &key)?,
                    None => value,
                };
                prev.insert(key, merged);
            }
            Ok(Value::Object(prev))
        }
        (Value::Array(_), Value::Array(next)) if property_key == "fallbackModel" => {
            Ok(Value::Array(next))
        }
        (Value::Array(mut prev), Value::Array(next)) => {
            for value in next {
                if !prev
                    .iter()
                    .any(|present| json_same_value_zero(present, &value))
                {
                    prev.push(value);
                }
            }
            Ok(Value::Array(prev))
        }
        (Value::Array(mut previous), Value::Object(source)) => {
            let previous_length = u32::try_from(previous.len()).map_err(|_| {
                Utf16JsonProjectionError::InvalidProjection(
                    "dense target exceeds JavaScript array length",
                )
            })?;
            let mut indexed = source
                .iter()
                .filter_map(|(key, value)| {
                    parse_json_array_index(key).map(|index| (index, key.clone(), value.clone()))
                })
                .collect::<Vec<_>>();
            indexed.sort_by_key(|(index, _, _)| *index);
            let max_source_len = indexed
                .last()
                .and_then(|(index, _, _)| index.checked_add(1))
                .unwrap_or(0);
            let pre_length = previous_length.max(max_source_len);
            let mut merged_indices = previous
                .drain(..)
                .enumerate()
                .map(|(index, value)| {
                    (
                        u32::try_from(index).expect("bounded by prior length"),
                        value,
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>();
            for (index, key, value) in indexed {
                let merged = match merged_indices.remove(&index) {
                    Some(previous) => merge_native_settings_value(previous, value, &key)?,
                    None => value,
                };
                merged_indices.insert(index, merged);
            }
            // `Object.keys` orders array indices before ordinary names, so the
            // `length` coercion/error occurs after every source-index merge.
            let result_length = match source.get("length") {
                Some(raw_length) => serde_array_length(raw_length)?,
                None => pre_length,
            };
            let result_length = usize::try_from(result_length).expect("u32 fits usize");
            let mut result = Vec::new();
            result
                .try_reserve_exact(result_length)
                .map_err(|error| Utf16JsonProjectionError::Materialization(error.to_string()))?;
            for index in 0..result_length {
                result.push(
                    merged_indices
                        .remove(&(index as u32))
                        .unwrap_or(Value::Null),
                );
            }
            Ok(Value::Array(result))
        }
        (Value::Object(_), Value::Array(next)) => Ok(Value::Array(next)),
        (_, next) => Ok(next),
    }
}

fn parse_json_array_index(key: &str) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    let index = key.parse::<u64>().ok()?;
    if index >= u64::from(u32::MAX) {
        return None;
    }
    u32::try_from(index).ok()
}

fn serde_array_length(value: &serde_json::Value) -> Result<u32, Utf16JsonProjectionError> {
    let projection = Utf16JsonProjection::from_serde_value(value)?;
    source_array_length_value(&projection)
}

fn json_same_value_zero(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Number(left), Value::Number(right)) => match (left.as_f64(), right.as_f64()) {
            (Some(left), Some(right)) => left == right || (left.is_nan() && right.is_nan()),
            _ => false,
        },
        (Value::String(left), Value::String(right)) => left == right,
        // The source uses `new Set`, which compares object and array values by
        // identity rather than by JSON structure.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::schema::SkillOverride;

    // Existing positive fixtures use the infallible-looking local shorthand;
    // production APIs below remain fallible and dedicated error cases call
    // `super::merge`/`super::merge_raw_layer` directly.
    fn merge(prev: SettingsJson, next: SettingsJson) -> SettingsJson {
        super::merge(prev, next).expect("valid settings merge fixture")
    }

    fn merge_raw_layer(
        prev: &mut std::collections::BTreeMap<String, serde_json::Value>,
        next: std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Vec<String> {
        super::merge_raw_layer(prev, next).expect("valid dense JSON fixture")
    }

    #[test]
    fn permission_arrays_accumulate_across_sources_in_typed_and_raw_views() {
        let lower = serde_json::json!({
            "permissions": {"allow":["Read", "Bash"], "deny":["WebFetch"],
                "custom":{"rules":["lower"]}}
        });
        let upper = serde_json::json!({
            "permissions": {"allow":["Bash", "Write"], "deny":["Read"],
                "custom":{"rules":["upper"]}}
        });
        let expected = serde_json::json!({
            "allow":["Read", "Bash", "Write"],
            "deny":["WebFetch", "Read"],
            "custom":{"rules":["lower", "upper"]},
        });
        let typed = merge(
            serde_json::from_value(lower.clone()).unwrap(),
            serde_json::from_value(upper.clone()).unwrap(),
        );
        assert_eq!(serde_json::to_value(typed.permissions).unwrap(), expected);
        let mut raw = std::collections::BTreeMap::new();
        let _ = merge_raw_layer(&mut raw, serde_json::from_value(lower).unwrap());
        let _ = merge_raw_layer(&mut raw, serde_json::from_value(upper).unwrap());
        assert_eq!(raw.get("permissions"), Some(&expected));
    }

    #[test]
    fn raw_source_merge_keeps_exact_strings_and_shared_strategies() {
        let mut previous = Utf16JsonProjection::parse(
            r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"mcp_tool","input":{"path":"\ud800"}}]}]},"trustedDirectories":["a","shared"],"permissions":{"allow":["Read"],"deny":["WebFetch"],"custom":{"rules":["lower"]}},"unknown":{"lower":1}}"#,
        )
        .expect("source settings layer");
        let next = Utf16JsonProjection::parse(
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"x"}]}]},"trustedDirectories":["shared","b"],"permissions":{"allow":["Bash","Read"],"deny":["Read"],"custom":{"rules":["upper"]}},"unknown":{"upper":2}}"#,
        )
        .expect("next source settings layer");

        let unioned = merge_raw_source_layer(&mut previous, &next).expect("source merge");

        assert_eq!(
            previous.string_units("/hooks/PostToolUse/0/hooks/0/input/path"),
            Some(vec![0xd800])
        );
        assert_eq!(
            previous.string_at("/hooks/Stop/0/hooks/0/command"),
            Some("x")
        );
        assert_eq!(
            previous
                .at_pointer("/trustedDirectories")
                .and_then(|array| array.get_index(0))
                .and_then(JsonValueView::as_str),
            Some("a")
        );
        assert_eq!(
            previous
                .at_pointer("/trustedDirectories")
                .and_then(|array| array.get_index(2))
                .and_then(JsonValueView::as_str),
            Some("b")
        );
        assert_eq!(
            previous
                .at_pointer("/permissions/allow")
                .and_then(|array| array.get_index(1))
                .and_then(JsonValueView::as_str),
            Some("Bash")
        );
        assert_eq!(
            previous
                .at_pointer("/permissions/custom/rules")
                .and_then(|array| array.get_index(1))
                .and_then(JsonValueView::as_str),
            Some("upper")
        );
        assert_eq!(previous.number_at("/unknown/upper"), Some(2.0));
        assert!(previous.at_pointer("/unknown/lower").is_none());
        assert!(unioned.contains(&"hooks".to_owned()));
        assert!(unioned.contains(&"trustedDirectories".to_owned()));
        assert!(!unioned.contains(&"unknown".to_owned()));
        assert!(previous
            .to_json_string()
            .expect("source output")
            .contains(r#""path":"\ud800""#));
    }

    #[test]
    fn plugin_configs_deep_merge_preserves_exact_strings_and_js_key_order() {
        let lower: SettingsJson = serde_json::from_str(
            r#"{"pluginConfigs":{"7":{"options":{"region":"us","keep":"\ud800"},"order":{"2":"two","10":"ten","x":"x"}},"z":{"old":true}}}"#,
        )
        .expect("parse lower plugin config source");
        let upper: SettingsJson = serde_json::from_str(
            r#"{"pluginConfigs":{"7":{"options":{"region":"eu","added":"\udfff"},"order":{"10":"TEN","1":"one"}},"2":{"from":"upper"}}}"#,
        )
        .expect("parse upper plugin config source");

        let merged = merge(lower, upper);
        let configs = merged.plugin_configs.expect("merged plugin configs");
        assert_eq!(configs.string_units("/7/options/keep"), Some(vec![0xd800]));
        assert_eq!(configs.string_units("/7/options/added"), Some(vec![0xdfff]));
        assert_eq!(configs.string_at("/7/options/region"), Some("eu"));
        assert!(configs.at_pointer("/z/old").unwrap().as_bool().unwrap());

        let json = configs.to_json_string().expect("stringify merged source");
        let order = json
            .split_once("\"order\":{")
            .expect("ordered nested object")
            .1;
        let one = order.find("\"1\"").unwrap();
        let two = order.find("\"2\"").unwrap();
        let ten = order.find("\"10\"").unwrap();
        let x = order.find("\"x\"").unwrap();
        assert!(
            one < two && two < ten && ten < x,
            "numeric keys must follow JavaScript order: {json}"
        );
        let root_two = json.find("\"2\"").unwrap();
        let root_seven = json.find("\"7\"").unwrap();
        let root_z = json.find("\"z\"").unwrap();
        assert!(
            root_two < root_seven && root_seven < root_z,
            "root numeric keys precede insertion-order strings: {json}"
        );
    }

    #[test]
    fn plugin_configs_accept_non_object_native_shapes_and_later_shape_wins() {
        let null: SettingsJson =
            serde_json::from_str(r#"{"pluginConfigs":null}"#).expect("accept native null");
        assert!(null
            .plugin_configs
            .as_ref()
            .unwrap()
            .at_pointer("")
            .unwrap()
            .is_null());

        let lower: SettingsJson =
            serde_json::from_str(r#"{"pluginConfigs":"lower"}"#).expect("accept native value");
        let upper: SettingsJson = serde_json::from_str(r#"{"pluginConfigs":[{"text":"\ud800"}]}"#)
            .expect("accept later native value");
        let merged = merge(lower, upper);
        let configs = merged.plugin_configs.expect("later source retained");
        assert_eq!(configs.array_len(""), Some(1));
        assert_eq!(configs.string_units("/0/text"), Some(vec![0xd800]));
    }

    #[test]
    fn raw_source_merge_matches_value_merge_for_json_representable_layers() {
        let previous = serde_json::json!({
            "hooks": {"PreToolUse": [{"matcher":"Bash"}]},
            "permissions": {"allow":["Read"],"custom":{"rules":["lower"]}},
            "trustedDirectories": ["a", "shared"],
            "unknown": {"lower": true}
        });
        let next = serde_json::json!({
            "hooks": {"PostToolUse": [{"matcher":"Write"}]},
            "permissions": {"allow":["Bash", "Read"],"custom":{"rules":["upper"]}},
            "trustedDirectories": ["shared", "b"],
            "unknown": {"upper": true}
        });
        let mut raw = std::collections::BTreeMap::new();
        let mut expected_unions = merge_raw_layer(
            &mut raw,
            serde_json::from_value(previous.clone()).expect("previous raw layer"),
        );
        expected_unions.extend(merge_raw_layer(
            &mut raw,
            serde_json::from_value(next.clone()).expect("next raw layer"),
        ));

        let mut source =
            Utf16JsonProjection::from_serde_value(&previous).expect("previous source layer");
        let next_source = Utf16JsonProjection::from_serde_value(&next).expect("next source layer");
        let source_unions =
            merge_raw_source_layer(&mut source, &next_source).expect("source settings merge");
        let source_value: serde_json::Value =
            serde_json::from_str(&source.to_json_string().expect("source JSON"))
                .expect("bounded JSON-compatible source view");

        assert_eq!(
            source_value,
            serde_json::Value::Object(raw.into_iter().collect())
        );
        assert_eq!(source_unions, expected_unions);
    }

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

    #[test]
    fn prompt_cache_ttl_settings_are_scalar_override() {
        let merged = merge(
            SettingsJson {
                prompt_cache_ttl: Some(crate::settings::schema::PromptCacheTtl::FiveMinutes),
                subagent_prompt_cache_ttl: Some(crate::settings::schema::PromptCacheTtl::OneHour),
                ..Default::default()
            },
            SettingsJson {
                prompt_cache_ttl: Some(crate::settings::schema::PromptCacheTtl::OneHour),
                subagent_prompt_cache_ttl: Some(
                    crate::settings::schema::PromptCacheTtl::FiveMinutes,
                ),
                ..Default::default()
            },
        );
        assert_eq!(
            merged.prompt_cache_ttl,
            Some(crate::settings::schema::PromptCacheTtl::OneHour)
        );
        assert_eq!(
            merged.subagent_prompt_cache_ttl,
            Some(crate::settings::schema::PromptCacheTtl::FiveMinutes)
        );
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
            output_style: Some(Utf16JsonProjection::parse(r#""old""#).unwrap()),
            language: Some(Utf16JsonProjection::parse(r#""English""#).unwrap()),
            proxy_auth_helper: Some(Utf16JsonProjection::parse(r#""/old/proxy-helper""#).unwrap()),
            telemetry_enabled: Some(false),
            enable_workflows: Some(true),
            workflow_size_guideline: Some("small".into()),
            ..Default::default()
        };
        let next = SettingsJson {
            model: Some("opus".into()),
            output_style: Some(Utf16JsonProjection::parse(r#""\ud800""#).unwrap()),
            language: Some(Utf16JsonProjection::parse(r#""Français""#).unwrap()),
            proxy_auth_helper: Some(Utf16JsonProjection::parse(r#""/new/proxy-helper""#).unwrap()),
            telemetry_enabled: None,
            enable_workflows: Some(false),
            workflow_size_guideline: Some("large".into()),
            ..Default::default()
        };
        let merged = merge(prev, next);
        assert_eq!(merged.model.as_deref(), Some("opus"));
        assert_eq!(
            merged
                .output_style
                .as_ref()
                .and_then(|value| value.string_units("")),
            Some(vec![0xd800]),
            "later outputStyle overrides earlier value without Unicode coercion"
        );
        assert_eq!(
            merged
                .language
                .as_ref()
                .and_then(|value| value.string_at("")),
            Some("Français")
        );
        assert_eq!(
            merged
                .proxy_auth_helper
                .as_ref()
                .and_then(|value| value.string_at("")),
            Some("/new/proxy-helper"),
            "proxyAuthHelper is a scalar and the later layer replaces the earlier one"
        );
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
    fn disable_bundled_skills_uses_scalar_later_source_wins() {
        let overridden = merge(
            SettingsJson {
                disable_bundled_skills: Some(true),
                ..Default::default()
            },
            SettingsJson {
                disable_bundled_skills: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(overridden.disable_bundled_skills, Some(false));

        let retained = merge(
            SettingsJson {
                disable_bundled_skills: Some(true),
                ..Default::default()
            },
            SettingsJson::default(),
        );
        assert_eq!(retained.disable_bundled_skills, Some(true));
    }

    #[test]
    fn scalar_proxy_auth_helper_preserves_previous_when_next_missing() {
        let merged = merge(
            SettingsJson {
                proxy_auth_helper: Some(
                    Utf16JsonProjection::parse(r#""/existing/proxy-helper""#).unwrap(),
                ),
                ..Default::default()
            },
            SettingsJson::default(),
        );

        assert_eq!(
            merged
                .proxy_auth_helper
                .as_ref()
                .and_then(|value| value.string_at("")),
            Some("/existing/proxy-helper")
        );
    }

    #[test]
    fn skill_overrides_deep_merge_by_exact_utf16_key_in_typed_and_raw_paths() {
        let lower_source = Utf16JsonProjection::parse(
            r#"{"skillOverrides":{"shared":"on","keep":"off","odd-\ud800":"name-only"}}"#,
        )
        .unwrap();
        let upper_source = Utf16JsonProjection::parse(
            r#"{"skillOverrides":{"shared":"user-invocable-only","added":"off"}}"#,
        )
        .unwrap();
        let lower: SettingsJson =
            serde_json::from_str(&lower_source.to_json_string().expect("valid lower source"))
                .expect("lower skill overrides parse");
        let upper: SettingsJson =
            serde_json::from_str(&upper_source.to_json_string().expect("valid upper source"))
                .expect("upper skill overrides parse");

        let merged = merge(lower, upper);
        let overrides = merged.skill_overrides.as_ref().expect("merged record");
        let units = |name: &str| name.encode_utf16().collect::<Vec<_>>();
        assert_eq!(
            overrides.get_units(&units("shared")),
            Some(SkillOverride::UserInvocableOnly)
        );
        assert_eq!(
            overrides.get_units(&units("keep")),
            Some(SkillOverride::Off)
        );
        assert_eq!(
            overrides.get_units(&units("added")),
            Some(SkillOverride::Off)
        );
        assert_eq!(
            overrides.get_units(
                &units("odd-")
                    .into_iter()
                    .chain([0xd800])
                    .collect::<Vec<_>>()
            ),
            Some(SkillOverride::NameOnly)
        );

        let mut raw = Utf16JsonProjection::parse("{}").unwrap();
        let _lower_unions = merge_raw_source_layer(&mut raw, &lower_source).unwrap();
        let _upper_unions = merge_raw_source_layer(&mut raw, &upper_source).unwrap();
        let raw_overrides = raw.subprojection("/skillOverrides").unwrap();
        assert_eq!(
            raw_overrides
                .subprojection_property_units(&units("shared"))
                .unwrap()
                .string_at(""),
            Some("user-invocable-only")
        );
        assert_eq!(
            raw_overrides
                .subprojection_property_units(
                    &units("odd-")
                        .into_iter()
                        .chain([0xd800])
                        .collect::<Vec<_>>(),
                )
                .unwrap()
                .string_at(""),
            Some("name-only")
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
    fn fusion_globals_merge_without_combining_strategy_configs() {
        let lower: SettingsJson = serde_json::from_value(serde_json::json!({
            "fusion": {"enabled": true, "defaultStrategyId": "a", "strategies": [
                {"id": "a", "name": "Review", "config": {"qualityPanelCount": 4}}
            ]}
        }))
        .unwrap();
        let upper: SettingsJson = serde_json::from_value(serde_json::json!({
            "fusion": {"allowedProfiles": ["openai"], "strategies": [
                {"id": "b", "name": "Review", "config": {"preset": "fast"}}
            ]}
        }))
        .unwrap();
        let merged = merge(lower, upper).fusion.unwrap();
        assert_eq!(merged.enabled, Some(true));
        assert_eq!(merged.default_strategy_id, None);
        assert_eq!(merged.strategies.len(), 1);
        assert_eq!(merged.strategies[0].id, "b");
        assert_eq!(merged.strategies[0].config.quality_panel_count, None);
        assert_eq!(merged.allowed_profiles, Some(vec!["openai".into()]));
    }

    #[test]
    fn fusion_source_globals_keep_utf16_without_inheriting_strategy_registry() {
        let mut lower = Utf16JsonProjection::parse(
            r#"{"fusion":{"enabled":true,"label":"\ud800","defaultStrategyId":"a","strategies":[{"id":"a","config":{"qualityPanelCount":4}}]}}"#,
        ).unwrap();
        let upper =
            Utf16JsonProjection::parse(r#"{"fusion":{"allowedProfiles":["openai"]}}"#).unwrap();
        merge_raw_source_layer(&mut lower, &upper).unwrap();
        assert_eq!(lower.string_units("/fusion/label"), Some(vec![0xd800]));
        assert_eq!(lower.array_len("/fusion/strategies"), Some(0));
        assert!(lower.subprojection("/fusion/defaultStrategyId").is_err());
        assert_eq!(
            lower.string_units("/fusion/allowedProfiles/0"),
            Some("openai".encode_utf16().collect())
        );
    }

    #[test]
    fn fusion_legacy_diagnostic_survives_higher_valid_globals() {
        let lower: SettingsJson = serde_json::from_value(serde_json::json!({
            "fusion": {"panelModels": []}
        }))
        .unwrap();
        let upper: SettingsJson = serde_json::from_value(serde_json::json!({
            "fusion": {"enabled": true}
        }))
        .unwrap();
        let merged = merge(lower, upper);
        merged
            .validate()
            .expect("Fusion errors must preserve unrelated settings");
        assert!(merged.fusion.unwrap().diagnostic.is_some());
    }

    #[test]
    fn new_238_settings_fields_deep_merge_policy_helpers_in_generic_settings_view() {
        use serde_json::json;
        use std::collections::BTreeMap;

        let prev_policy_helpers = Utf16JsonProjection::parse(
            r#"{"defaultSettings":{"model":"base"},"linux":{"path":"/usr/bin/base-helper"}}"#,
        )
        .expect("previous policy helper source");
        let next_policy_helpers =
            Utf16JsonProjection::parse(r#"{"macos":{"path":"/opt/macos-helper"}}"#)
                .expect("next policy helper source");
        let prev_policy_helper = Utf16JsonProjection::parse(r#"{"path":"/usr/bin/singular"}"#)
            .expect("previous singular helper source");
        let next_policy_helper = Utf16JsonProjection::parse(r#"{"path":"/opt/singular"}"#)
            .expect("next singular helper source");

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
                policy_helper: Some(prev_policy_helper),
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
                policy_helpers: Some(next_policy_helpers.clone()),
                policy_helper: Some(next_policy_helper.clone()),
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

        assert_eq!(
            merged.policy_helpers,
            Some(
                Utf16JsonProjection::parse(
                    r#"{"defaultSettings":{"model":"base"},"linux":{"path":"/usr/bin/base-helper"},"macos":{"path":"/opt/macos-helper"}}"#,
                )
                .expect("merged policy helper projection")
            )
        );
        assert_eq!(merged.policy_helper, Some(next_policy_helper));

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
    fn policy_helpers_use_native_merge_with_set_and_fallback_model_semantics() {
        const LOWER: &str = r#"{"linux":{"defaultSettings":{"allowedHttpHookUrls":["https://shared.example/*","https://lower.example/*"],"fallbackModel":["lower"],"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo same"}]},{"hooks":[{"type":"command","command":"echo same"}]}]}}}}"#;
        const UPPER: &str = r#"{"linux":{"defaultSettings":{"allowedHttpHookUrls":["https://shared.example/*","https://upper.example/*"],"fallbackModel":["upper"],"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo same"}]}]}}}}"#;
        let expected = Utf16JsonProjection::parse(
            r#"{"linux":{"defaultSettings":{"allowedHttpHookUrls":["https://shared.example/*","https://lower.example/*","https://upper.example/*"],"fallbackModel":["upper"],"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo same"}]},{"hooks":[{"type":"command","command":"echo same"}]},{"hooks":[{"type":"command","command":"echo same"}]}]}}}}"#,
        )
        .expect("Native policy helper merge result");
        let lower = Utf16JsonProjection::parse(LOWER).expect("lower helper settings");
        let upper = Utf16JsonProjection::parse(UPPER).expect("upper helper settings");

        let typed = merge(
            SettingsJson {
                policy_helpers: Some(lower.clone()),
                ..Default::default()
            },
            SettingsJson {
                policy_helpers: Some(upper.clone()),
                ..Default::default()
            },
        );
        assert!(source_values_equal(
            typed
                .policy_helpers
                .as_ref()
                .expect("typed helper projection"),
            &expected
        ));

        let source_layer = |policy_helpers: Utf16JsonProjection| {
            Utf16JsonProjection::object(vec![("policyHelpers".to_owned(), policy_helpers)])
                .expect("source layer object")
        };
        let mut source_merged = Utf16JsonProjection::parse("{}").expect("empty source object");
        let _lower_unions =
            merge_raw_source_layer(&mut source_merged, &source_layer(lower.clone()))
                .expect("lower source layer");
        let upper_unions = merge_raw_source_layer(&mut source_merged, &source_layer(upper.clone()))
            .expect("upper source layer");
        assert!(upper_unions.iter().any(|key| key == "policyHelpers"));
        assert!(source_values_equal(
            &source_merged
                .subprojection("/policyHelpers")
                .expect("raw source helper projection"),
            &expected
        ));

        let raw_layer = |policy_helpers: &str| {
            serde_json::from_str::<std::collections::BTreeMap<String, serde_json::Value>>(&format!(
                r#"{{"policyHelpers":{policy_helpers}}}"#
            ))
            .expect("raw settings layer")
        };
        let mut raw_merged = std::collections::BTreeMap::new();
        let _lower_unions = merge_raw_layer(&mut raw_merged, raw_layer(LOWER));
        let upper_unions = merge_raw_layer(&mut raw_merged, raw_layer(UPPER));
        assert!(upper_unions.iter().any(|key| key == "policyHelpers"));
        let raw_projection = Utf16JsonProjection::parse(
            &serde_json::to_string(raw_merged.get("policyHelpers").expect("raw helper value"))
                .expect("serialize raw helper value"),
        )
        .expect("raw helper projection");
        assert!(source_values_equal(&raw_projection, &expected));

        let zero = Utf16JsonProjection::parse("0").unwrap();
        let negative_zero = Utf16JsonProjection::parse("-0").unwrap();
        let infinity = Utf16JsonProjection::parse("1e999").unwrap();
        let same_infinity = Utf16JsonProjection::parse("1e999").unwrap();
        assert!(source_same_value_zero(
            zero.at_pointer("").unwrap(),
            negative_zero.at_pointer("").unwrap()
        ));
        assert!(source_same_value_zero(
            infinity.at_pointer("").unwrap(),
            same_infinity.at_pointer("").unwrap()
        ));

        let lone_surrogate = Utf16JsonProjection::root_string("�".into(), vec![0xd800]).unwrap();
        let same_lone_surrogate =
            Utf16JsonProjection::root_string("�".into(), vec![0xd800]).unwrap();
        assert!(source_same_value_zero(
            lone_surrogate.at_pointer("").unwrap(),
            same_lone_surrogate.at_pointer("").unwrap()
        ));

        let object = Utf16JsonProjection::parse(r#"{"same":true}"#).unwrap();
        let same_shape_object = Utf16JsonProjection::parse(r#"{"same":true}"#).unwrap();
        assert!(!source_same_value_zero(
            object.at_pointer("").unwrap(),
            same_shape_object.at_pointer("").unwrap()
        ));
    }

    #[test]
    fn native_n3_special_keys_and_mixed_array_object_merge_match_all_settings_views() {
        const LOWER: &str = r#"{"linux":{"defaultSettings":{"modelPicker":{"model":"old","options":[{"old":true},"keep"]},"extraKnownMarketplaces":{"corp":{"old":true,"nested":{"old":1}},"keep":{"kept":true}},"managedMcpServers":{"corp":{"old":true,"nested":{"old":1}},"keep":{"kept":true}},"arrayTarget":[1,{"old":true}],"holeTarget":[1],"lengthTarget":[1,2],"objectTarget":{"existing":{"old":true}}}}}"#;
        const UPPER: &str = r#"{"linux":{"defaultSettings":{"modelPicker":{"model":"new","options":[{"new":true}]},"extraKnownMarketplaces":{"corp":{"new":true,"nested":{"new":2}},"added":{"added":true}},"managedMcpServers":{"corp":{"new":true,"nested":{"new":2}},"added":{"added":true}},"arrayTarget":{"0":9,"2":"tail","other":"json omits this array property"},"holeTarget":{"3":"tail","extra":"not output"},"lengthTarget":{"length":0,"0":9},"objectTarget":[{"new":true}]}}}"#;
        const EXPECTED: &str = r#"{"linux":{"defaultSettings":{"modelPicker":{"model":"new","options":[{"new":true}]},"extraKnownMarketplaces":{"corp":{"new":true,"nested":{"new":2}},"keep":{"kept":true},"added":{"added":true}},"managedMcpServers":{"corp":{"new":true,"nested":{"new":2}},"keep":{"kept":true},"added":{"added":true}},"arrayTarget":[9,{"old":true},"tail"],"holeTarget":[1,null,null,"tail"],"lengthTarget":[],"objectTarget":[{"new":true}]}}}"#;
        let lower = Utf16JsonProjection::parse(LOWER).expect("lower policy helper settings");
        let upper = Utf16JsonProjection::parse(UPPER).expect("upper policy helper settings");
        let expected = Utf16JsonProjection::parse(EXPECTED).expect("expected Native N3 result");

        let typed = merge(
            SettingsJson {
                policy_helpers: Some(lower.clone()),
                ..Default::default()
            },
            SettingsJson {
                policy_helpers: Some(upper.clone()),
                ..Default::default()
            },
        );
        let typed_policy_helpers = typed.policy_helpers.as_ref().expect("typed policy helpers");
        assert_eq!(
            typed_policy_helpers
                .to_json_string()
                .expect("serialize typed helpers"),
            EXPECTED,
            "Native JSON output fills array holes with null and omits array named properties"
        );

        let assert_native_sparse_state = |helpers: &Utf16JsonProjection| {
            let array_target = helpers
                .subprojection("/linux/defaultSettings/arrayTarget")
                .expect("merged array target");
            assert_eq!(array_target.array_len(""), Some(3));
            assert_eq!(
                array_target
                    .array_named_properties()
                    .expect("array target own properties")
                    .iter()
                    .map(|entry| entry.display_key)
                    .collect::<Vec<_>>(),
                vec!["other"],
                "Nf retains an enumerable named property on the target array"
            );
            assert_eq!(
                array_target
                    .subprojection_array_property_units(&"other".encode_utf16().collect::<Vec<_>>())
                    .expect("array target named value")
                    .string_at(""),
                Some("json omits this array property")
            );

            let hole_target = helpers
                .subprojection("/linux/defaultSettings/holeTarget")
                .expect("merged hole target");
            let (logical_length, entries) = hole_target.array_entries().expect("sparse entries");
            assert_eq!(logical_length, 4);
            assert_eq!(
                entries.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
                vec![0, 3],
                "the source projection keeps holes absent rather than materializing null"
            );
            assert!(hole_target
                .at_pointer("/1")
                .expect("first in-range hole")
                .is_undefined());
            assert!(hole_target
                .at_pointer("/2")
                .expect("second in-range hole")
                .is_undefined());
            assert_eq!(
                hole_target
                    .array_named_properties()
                    .expect("hole target own properties")
                    .iter()
                    .map(|entry| entry.display_key)
                    .collect::<Vec<_>>(),
                vec!["extra"]
            );
            assert_eq!(
                hole_target
                    .subprojection_array_property_units(&"extra".encode_utf16().collect::<Vec<_>>())
                    .expect("hole target named value")
                    .string_at(""),
                Some("not output")
            );
        };
        assert_native_sparse_state(typed_policy_helpers);

        let source_layer = |policy_helpers: Utf16JsonProjection| {
            Utf16JsonProjection::object(vec![("policyHelpers".to_owned(), policy_helpers)])
                .expect("source layer")
        };
        let mut raw_source = Utf16JsonProjection::parse("{}").expect("empty source layer");
        merge_raw_source_layer(&mut raw_source, &source_layer(lower.clone()))
            .expect("lower raw source merge");
        merge_raw_source_layer(&mut raw_source, &source_layer(upper.clone()))
            .expect("upper raw source merge");
        let raw_source_helpers = raw_source
            .subprojection("/policyHelpers")
            .expect("raw source policy helpers");
        assert_eq!(
            raw_source_helpers
                .to_json_string()
                .expect("serialize raw source helpers"),
            EXPECTED,
            "source-preserving JSON output matches Native after sparse-array serialization"
        );
        assert_native_sparse_state(&raw_source_helpers);

        let raw_layer = |helpers: &str| {
            serde_json::from_str::<std::collections::BTreeMap<String, serde_json::Value>>(&format!(
                r#"{{"policyHelpers":{helpers}}}"#
            ))
            .expect("raw JSON layer")
        };
        let mut raw_json = std::collections::BTreeMap::new();
        let _ = merge_raw_layer(&mut raw_json, raw_layer(LOWER));
        let _ = merge_raw_layer(&mut raw_json, raw_layer(UPPER));
        let raw_json_projection = Utf16JsonProjection::parse(
            &serde_json::to_string(raw_json.get("policyHelpers").expect("merged raw helpers"))
                .expect("serialize raw policy helpers"),
        )
        .expect("raw JSON policy helper projection");
        assert!(source_values_equal(&raw_json_projection, &expected));

        // These special N3 rules also apply to top-level settings merges. The
        // object values collide by key, so the later whole value wins.
        let top_lower = Utf16JsonProjection::parse(
            r#"{"modelPicker":{"old":true},"extraKnownMarketplaces":{"corp":{"old":true,"nested":{"old":1}},"keep":{"kept":true}},"managedMcpServers":{"corp":{"old":true,"nested":{"old":1}},"keep":{"kept":true}}}"#,
        )
        .unwrap();
        let top_upper = Utf16JsonProjection::parse(
            r#"{"modelPicker":{"new":true},"extraKnownMarketplaces":{"corp":{"new":true,"nested":{"new":2}},"added":{"added":true}},"managedMcpServers":{"corp":{"new":true,"nested":{"new":2}},"added":{"added":true}}}"#,
        )
        .unwrap();
        let mut top = Utf16JsonProjection::parse("{}").unwrap();
        merge_raw_source_layer(&mut top, &top_lower).unwrap();
        merge_raw_source_layer(&mut top, &top_upper).unwrap();
        assert!(source_values_equal(
            &top.subprojection("/modelPicker").unwrap(),
            &Utf16JsonProjection::parse(r#"{"new":true}"#).unwrap()
        ));
        assert!(source_values_equal(
            &top.subprojection("/extraKnownMarketplaces/corp").unwrap(),
            &Utf16JsonProjection::parse(r#"{"new":true,"nested":{"new":2}}"#).unwrap()
        ));
        assert!(source_values_equal(
            &top.subprojection("/managedMcpServers/corp").unwrap(),
            &Utf16JsonProjection::parse(r#"{"new":true,"nested":{"new":2}}"#).unwrap()
        ));

        let top_json_layer = |source: &str| {
            serde_json::from_str::<std::collections::BTreeMap<String, serde_json::Value>>(source)
                .expect("top-level raw JSON layer")
        };
        let mut top_json = std::collections::BTreeMap::new();
        let _ = merge_raw_layer(
            &mut top_json,
            top_json_layer(
                r#"{"modelPicker":{"old":true},"extraKnownMarketplaces":{"corp":{"old":true,"nested":{"old":1}},"keep":{"kept":true}},"managedMcpServers":{"corp":{"old":true,"nested":{"old":1}},"keep":{"kept":true}}}"#,
            ),
        );
        let _ = merge_raw_layer(
            &mut top_json,
            top_json_layer(
                r#"{"modelPicker":{"new":true},"extraKnownMarketplaces":{"corp":{"new":true,"nested":{"new":2}},"added":{"added":true}},"managedMcpServers":{"corp":{"new":true,"nested":{"new":2}},"added":{"added":true}}}"#,
            ),
        );
        assert_eq!(
            top_json.get("modelPicker"),
            Some(&serde_json::json!({"new": true}))
        );
        assert_eq!(
            top_json
                .get("extraKnownMarketplaces")
                .and_then(|value| value.get("corp")),
            Some(&serde_json::json!({"new": true, "nested": {"new": 2}}))
        );
        assert_eq!(
            top_json
                .get("managedMcpServers")
                .and_then(|value| value.get("corp")),
            Some(&serde_json::json!({"new": true, "nested": {"new": 2}}))
        );

        let parsed_lower: SettingsJson = serde_json::from_str(
            r#"{"extraKnownMarketplaces":{"corp":{"old":true,"nested":{"old":1}},"keep":{"kept":true}}}"#,
        )
        .unwrap();
        let parsed_upper: SettingsJson = serde_json::from_str(
            r#"{"extraKnownMarketplaces":{"corp":{"new":true,"nested":{"new":2}},"added":{"added":true}}}"#,
        )
        .unwrap();
        let typed_top = merge(parsed_lower, parsed_upper);
        assert_eq!(
            typed_top
                .extra_known_marketplaces
                .as_ref()
                .and_then(|values| values.get("corp")),
            Some(&serde_json::json!({"new": true, "nested": {"new": 2}}))
        );
        assert!(typed_top
            .extra_known_marketplaces
            .as_ref()
            .is_some_and(|values| values.contains_key("keep") && values.contains_key("added")));
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
                json!({"enabled": true}),
                json!({"proactive": true, "strategies": []}),
            ),
            (
                "modelSettings",
                json!({"claude-opus-4-7": {"maxEffortLevel": "high"}}),
                json!({"claude-sonnet-4-5": {"effortLevel": "medium"}}),
            ),
            (
                "skillOverrides",
                json!({"lower": "on"}),
                json!({"upper": "off"}),
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
            serde_json::from_str(&serde_json::to_string(&lower).unwrap())
                .expect("the fixture must parse as SettingsJson, or it is not testing `merge`");
        let typed_upper: SettingsJson =
            serde_json::from_str(&serde_json::to_string(&upper).unwrap())
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
                json!({"enabled": true, "strategies": []}),
                json!({"proactive": true, "strategies": []}),
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
            // `skillListingMaxDescChars` is modeled as f64, so its typed
            // serde round trip preserves the float-form JSON numbers. Use
            // integer-valued floats to meet Native's positive-integer schema.
            (json!(3.0), json!(2.0)),
            // `skillListingBudgetFraction` is strictly positive and at most
            // one, so the integer pair above cannot exercise it. Keep a pair
            // within the Native numeric schema to measure scalar override.
            (json!(0.25), json!(0.75)),
            // String ENUMS reject the generic `"lower"`/`"upper"` pair, so they
            // need real variants. Appended last on purpose: the search takes the
            // first shape that round-trips, so this can only ever be reached by a
            // field for which every shape above already failed — which is the
            // case that used to panic.
            (json!("auto"), json!("tmux")),
            (json!("china_mainland"), json!("international")),
            (json!("low"), json!("medium")),
            (json!("5m"), json!("1h")),
            // `skillOverrides` is a record whose values are a string enum.
            (json!({"lower": "on"}), json!({"upper": "off"})),
        ];

        // Whether `{field: value}` survives a `SettingsJson` round trip
        // unchanged — i.e. whether this probe shape is the field's real type.
        // A wrong-typed known field fails to parse; an unknown field parses
        // but vanishes from the re-serialization, so both are rejected.
        let round_trips = |field: &str, value: &Value| -> bool {
            let source = json!({ field: value }).to_string();
            let Ok(parsed) = serde_json::from_str::<SettingsJson>(&source) else {
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
                serde_json::from_str(&json!({ field: lower }).to_string())
                    .expect("probe must parse");
            let typed_upper: SettingsJson =
                serde_json::from_str(&json!({ field: upper }).to_string())
                    .expect("probe must parse");
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
            merged.commit,
            Some("user commit".into()),
            "the later layer named only `pr`, so `commit` must survive"
        );
        assert_eq!(merged.pr, Some("project pr".into()));
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
                commit: Some(String::new().into()),
                pr: None,
            }),
            ..Default::default()
        };
        let merged = merge(user, project).attribution.expect("attribution");
        assert_eq!(
            merged.commit,
            Some(String::new().into()),
            "an explicit empty must override, i.e. disable the trailer"
        );
    }

    #[test]
    fn attribution_merge_preserves_exact_surrogate_projection() {
        use crate::settings::schema::Attribution;
        let exact = Utf16JsonProjection::parse(r#""\ud800""#).expect("source string");
        let user = SettingsJson {
            attribution: Some(Attribution {
                commit: Some(exact.clone()),
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
        assert_eq!(merged.commit, Some(exact));
        assert_eq!(
            merged
                .commit
                .as_ref()
                .expect("commit string")
                .at_pointer("")
                .expect("root value")
                .code_units(),
            Some([0xd800_u16].as_slice())
        );
    }

    #[test]
    fn n3_set_union_keeps_first_undefined_hole_separate_from_null() {
        let lower = Utf16JsonProjection::sparse_array(
            3,
            vec![
                (0, Utf16JsonProjection::parse("1").unwrap()),
                (2, Utf16JsonProjection::parse("3").unwrap()),
            ],
        )
        .unwrap();
        let upper = Utf16JsonProjection::parse("[1,null,3]").unwrap();
        let merged = merge_native_settings_source_value(lower, &upper, "custom").unwrap();

        assert_eq!(merged.array_len(""), Some(4));
        assert_eq!(merged.to_json_string().unwrap(), "[1,null,3,null]");
        assert!(merged.at_pointer("/1").unwrap().is_undefined());
        assert!(merged.at_pointer("/3").unwrap().is_null());
        let entries = merged.array_entries().unwrap().1;
        assert_eq!(
            entries.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            [0, 2, 3]
        );
    }

    #[test]
    fn native_array_target_keeps_named_state_for_a_later_nested_length_error() {
        let lower = Utf16JsonProjection::parse(r#"{"x":[]}"#).unwrap();
        let first = Utf16JsonProjection::parse(r#"{"x":{"foo":[]}}"#).unwrap();
        let upper = Utf16JsonProjection::parse(r#"{"x":{"foo":{"length":1.5}}}"#).unwrap();

        let state = merge_native_settings_source_value(lower, &first, "").unwrap();
        let array = state.at_pointer("/x").unwrap();
        assert!(array.array_len().is_some());
        assert_eq!(array.get_property("foo").unwrap().array_len(), Some(0));
        assert_eq!(state.to_json_string().unwrap(), r#"{"x":[]}"#);

        let error = merge_native_settings_source_value(state, &upper, "").unwrap_err();
        assert_eq!(error.to_string(), "RangeError: Invalid array length");
        assert!(matches!(
            error,
            Utf16JsonProjectionError::InvalidArraySetLength { .. }
        ));
    }

    #[test]
    fn object_merge_reports_array_length_error_in_native_source_key_order() {
        let previous = Utf16JsonProjection::parse(r#"{"z":[],"a":[]}"#).unwrap();
        let next =
            Utf16JsonProjection::parse(r#"{"a":{"length":1.5},"z":{"length":2.5}}"#).unwrap();
        let error = merge_native_settings_source_value(previous, &next, "").unwrap_err();
        let Utf16JsonProjectionError::InvalidArraySetLength { raw_value, .. } = error else {
            panic!("Native first source-key failure should be ArraySetLength");
        };
        assert_eq!(raw_value.to_json_string().unwrap(), "1.5");
    }

    #[test]
    fn object_merge_orders_integer_source_keys_before_other_keys() {
        let previous = Utf16JsonProjection::parse(r#"{"10":[],"2":[]}"#).unwrap();
        let next =
            Utf16JsonProjection::parse(r#"{"10":{"length":2.5},"2":{"length":1.5}}"#).unwrap();
        let error = merge_native_settings_source_value(previous, &next, "").unwrap_err();
        let Utf16JsonProjectionError::InvalidArraySetLength { raw_value, .. } = error else {
            panic!("Native first source-key failure should be ArraySetLength");
        };
        assert_eq!(raw_value.to_json_string().unwrap(), "1.5");
    }

    #[test]
    fn native_safe_merge_keys_match_proto_constructor_and_inherited_targets() {
        let empty = Utf16JsonProjection::parse("{}").unwrap();
        let root_proto =
            Utf16JsonProjection::parse(r#"{"__proto__":{"nested":{"ignored":true}}}"#).unwrap();
        let merged = merge_native_settings_source_value(empty.clone(), &root_proto, "").unwrap();
        assert_eq!(merged.to_json_string().unwrap(), "{}");

        let lower = Utf16JsonProjection::parse(
            r#"{"wrap":{"__proto__":{"nested":{"lower":1}},"constructor":{"prototype":{"lower":1}},"toString":{"lower":1}}}"#,
        )
        .unwrap();
        let upper = Utf16JsonProjection::parse(
            r#"{"wrap":{"__proto__":{"nested":{"upper":2}},"constructor":{"prototype":{"upper":2}},"toString":{"upper":2}}}"#,
        )
        .unwrap();
        let lower_state = merge_native_settings_source_value(empty, &lower, "").unwrap();
        let merged = merge_native_settings_source_value(lower_state, &upper, "").unwrap();
        assert_eq!(
            merged.to_json_string().unwrap(),
            r#"{"wrap":{"__proto__":{"nested":{"lower":1}},"constructor":{"prototype":{"lower":1,"upper":2}},"toString":{"lower":1,"upper":2}}}"#
        );

        let proto_scalar = Utf16JsonProjection::parse(r#"{"__proto__":1}"#).unwrap();
        let merged = merge_native_settings_source_value(
            Utf16JsonProjection::parse("{}").unwrap(),
            &proto_scalar,
            "",
        )
        .unwrap();
        assert_eq!(merged.to_json_string().unwrap(), r#"{"__proto__":1}"#);

        let lower = Utf16JsonProjection::parse(r#"{"wrap":{"__proto__":7}}"#).unwrap();
        let upper = Utf16JsonProjection::parse(r#"{"wrap":{"__proto__":9}}"#).unwrap();
        let lower_state = merge_native_settings_source_value(
            Utf16JsonProjection::parse("{}").unwrap(),
            &lower,
            "",
        )
        .unwrap();
        let merged = merge_native_settings_source_value(lower_state, &upper, "").unwrap();
        assert_eq!(
            merged.to_json_string().unwrap(),
            r#"{"wrap":{"__proto__":9}}"#
        );

        let sparse = Utf16JsonProjection::sparse_array(0, vec![]).unwrap();
        let scalar = Utf16JsonProjection::parse(r#"{"__proto__":7}"#).unwrap();
        let array_with_proto =
            merge_native_settings_source_value(sparse, &scalar, "custom").unwrap();
        assert_eq!(array_with_proto.to_json_string().unwrap(), "[]");
        assert_eq!(array_with_proto.number_at("/__proto__"), Some(7.0));

        let composite = Utf16JsonProjection::parse(r#"{"__proto__":{"ignored":true}}"#).unwrap();
        let array_with_proto =
            merge_native_settings_source_value(array_with_proto, &composite, "custom").unwrap();
        assert_eq!(array_with_proto.number_at("/__proto__"), Some(7.0));
    }

    #[test]
    fn object_indices_and_to_number_length_form_a_sparse_array_without_dense_cap() {
        let previous = Utf16JsonProjection::sparse_array(0, vec![]).unwrap();
        let source = Utf16JsonProjection::parse(r#"{"4294967294":"last"}"#).unwrap();
        let merged = merge_native_settings_source_value(previous, &source, "custom").unwrap();
        assert_eq!(merged.array_len(""), Some(u32::MAX as usize));
        assert_eq!(merged.string_at("/4294967294"), Some("last"));
        assert_eq!(
            merged.array_entries().unwrap().1[0].0,
            u32::MAX - 1,
            "storage grows with own entries, not the logical array length"
        );

        let previous = Utf16JsonProjection::parse("[7,8]").unwrap();
        let source = Utf16JsonProjection::parse(r#"{"length":"0","0":1}"#).unwrap();
        let merged = merge_native_settings_source_value(previous, &source, "custom").unwrap();
        assert_eq!(merged.to_json_string().unwrap(), "[]");
    }

    #[test]
    fn array_set_length_uses_native_json_tonumber_cases_and_throws_range_error() {
        for (json, expected) in [
            ("0", 0),
            (r#""0""#, 0),
            ("null", 0),
            ("true", 1),
            ("[]", 0),
            ("[2]", 2),
            ("[null]", 0),
            ("[[2]]", 2),
            ("[[null]]", 0),
        ] {
            let projection = Utf16JsonProjection::parse(json).unwrap();
            assert_eq!(
                source_array_length_value(&projection).unwrap(),
                expected,
                "{json}"
            );
        }
        for json in [r#""bad""#, "{}", "1.5", "-1", "[true]", "[2,3]", "[[2,3]]"] {
            let projection = Utf16JsonProjection::parse(json).unwrap();
            let error = source_array_length_value(&projection).unwrap_err();
            assert_eq!(error.to_string(), "RangeError: Invalid array length");
            assert!(matches!(
                error,
                Utf16JsonProjectionError::InvalidArraySetLength { .. }
            ));
        }

        // A huge sparse source array has a joined comma string and converts to
        // NaN without materializing its logical length.
        let sparse = Utf16JsonProjection::sparse_array(u32::MAX, Vec::new()).unwrap();
        assert!(matches!(
            source_array_length_value(&sparse),
            Err(Utf16JsonProjectionError::InvalidArraySetLength { .. })
        ));

        // A one-element hole joins to the empty string, unlike a multi-item
        // sparse array whose comma makes the numeric conversion invalid.
        let one_hole = Utf16JsonProjection::sparse_array(1, Vec::new()).unwrap();
        assert_eq!(source_array_length_value(&one_hole).unwrap(), 0);

        // ToPrimitive failures happen before ArraySetLength validates a
        // number. Bun 1.4.3 reports this JSON object's non-callable own
        // `toString` as `TypeError: No default value`.
        let non_callable_to_string = Utf16JsonProjection::parse(r#"{"toString":1}"#).unwrap();
        let error = source_array_length_value(&non_callable_to_string).unwrap_err();
        assert_eq!(error.to_string(), "TypeError: No default value");
        assert!(matches!(
            error,
            Utf16JsonProjectionError::InvalidArrayLengthPrimitive { .. }
        ));
        let non_callable_value_of = Utf16JsonProjection::parse(r#"{"valueOf":1}"#).unwrap();
        assert!(matches!(
            source_array_length_value(&non_callable_value_of),
            Err(Utf16JsonProjectionError::InvalidArraySetLength { .. })
        ));

        let nested_non_callable_to_string =
            Utf16JsonProjection::parse(r#"[{"toString":1}]"#).unwrap();
        assert!(matches!(
            source_array_length_value(&nested_non_callable_to_string),
            Err(Utf16JsonProjectionError::InvalidArrayLengthPrimitive { .. })
        ));

        let settings_error = crate::settings::SettingsError::Projection(
            Utf16JsonProjectionError::InvalidArrayLengthPrimitive {
                raw_value: Box::new(non_callable_to_string.clone()),
            },
        );
        let diagnostic = settings_error.diagnostic_projection();
        assert_eq!(
            diagnostic
                .subprojection("/sourceValue")
                .unwrap()
                .to_json_string()
                .unwrap(),
            r#"{"toString":1}"#
        );
    }
}
