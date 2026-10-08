use super::{
    negotiated_era_label, negotiated_protocol_from_cache_entry, operation_guard_rejected,
    DiscoveryCacheConsult, DiscoveryCachePartition, GrantProvenance, McpCatalogChanged,
    McpCatalogKind, McpOperationGuard, McpRegistry,
};
use crate::connection::{ConfigScope, McpConnectionState, McpServerConfig};
use crate::oauth;
use lingxi_core::host::{McpError, McpTransportSpec, ServerCapabilitiesDto};
use lingxi_core::types::McpConnectionId;

impl McpRegistry {
    /// Called only by the current lazy-dial owner while holding the server's
    /// lifecycle lock. Compare the grant again before mutating its partition:
    /// Claude 2.1.286 WRt refuses strikes fetched under a superseded grant.
    pub(super) async fn record_discovery_cache_refresh_failure_locked(
        &self,
        config: &McpServerConfig,
        partition: Option<&DiscoveryCachePartition>,
        auth_response: bool,
    ) {
        let Some(partition) = partition else {
            return;
        };
        let current = self
            .discovery_cache_partition_for(config, partition.negotiation_mode)
            .await;
        if current.as_ref().ok() != Some(partition) {
            return;
        }
        if auth_response {
            // The upstream auth classifier purges a rejected credential's
            // cached catalog before adopting the needs-auth row.
            if let Some(store) = &self.discovery_cache_store {
                if let Err(error) = store.purge_partitioned(&partition.partition_key) {
                    tracing::warn!(server = %config.name, %error, "Discovery cache auth purge skipped");
                }
            }
        } else {
            self.record_discovery_cache_refresh_failure(config, partition);
        }
    }
    pub(super) async fn discovery_cache_partition_for(
        &self,
        config: &McpServerConfig,
        negotiation_mode: crate::protocol_negotiation::NegotiationMode,
    ) -> Result<DiscoveryCachePartition, crate::discovery_cache::MissReason> {
        let grant_provenance = self.current_grant_provenance(config).await?;
        self.discovery_cache_partition_for_grant(
            config,
            negotiation_mode,
            grant_provenance.as_ref(),
        )
        .await
    }
    pub(super) async fn discovery_cache_partition_for_grant(
        &self,
        config: &McpServerConfig,
        negotiation_mode: crate::protocol_negotiation::NegotiationMode,
        grant_provenance: Option<&GrantProvenance>,
    ) -> Result<DiscoveryCachePartition, crate::discovery_cache::MissReason> {
        // Agent catalogs are safe to cache only when their stable source is
        // present.  A missing source fails closed rather than sharing an
        // agent-scoped catalog under the plain server name/spec.
        if config.scope == ConfigScope::Agent && config.metadata.agent_source.is_none() {
            return Err(crate::discovery_cache::MissReason::NoFingerprint);
        }
        let Some(grant_provenance) = grant_provenance else {
            return Err(crate::discovery_cache::MissReason::NoFingerprint);
        };
        let logical_key = crate::discovery_cache::logical_cache_key(config);
        let expected_era = match negotiation_mode {
            crate::protocol_negotiation::NegotiationMode::Auto { .. } => "modern",
            crate::protocol_negotiation::NegotiationMode::Legacy => "legacy",
        };
        let partition_key = crate::discovery_cache::partition_key_for_era(
            &logical_key,
            &grant_provenance.fingerprint,
            expected_era,
        );
        Ok(DiscoveryCachePartition {
            logical_key,
            partition_key,
            expected_era,
            negotiation_mode,
        })
    }
    pub(super) async fn current_grant_provenance(
        &self,
        config: &McpServerConfig,
    ) -> Result<Option<GrantProvenance>, crate::discovery_cache::MissReason> {
        let has_oauth = matches!(
            &config.spec,
            McpTransportSpec::Sse { oauth: Some(_), .. }
                | McpTransportSpec::Http { oauth: Some(_), .. }
        );
        if !has_oauth {
            return Ok(Some(GrantProvenance::unbound()));
        }
        let Some(deps) = &self.oauth else {
            return Ok(None);
        };
        let key = oauth::server_key(&config.name, &config.spec);
        let grant_token = oauth::discovery_cache_grant_token(&deps.storage, &key)
            .await
            .map_err(|_| crate::discovery_cache::MissReason::NoFingerprint)?;
        let Some(grant_token) = grant_token else {
            return Ok(None);
        };
        Ok(Some(GrantProvenance::from_grant_token(&grant_token, true)))
    }
    pub(super) async fn discovery_cache_secret_candidates_for(
        &self,
        config: &McpServerConfig,
    ) -> Result<Vec<String>, ()> {
        let mut candidates = Self::config_secret_candidates(config);
        if let Some(deps) = &self.oauth {
            if matches!(
                config.spec,
                McpTransportSpec::Sse { .. } | McpTransportSpec::Http { .. }
            ) {
                let server_key = oauth::server_key(&config.name, &config.spec);
                let stored = oauth::load_tokens(&deps.storage, &server_key)
                    .await
                    .map_err(|_| ())?;
                if let Some(stored) = stored {
                    candidates.push(stored.access_token);
                    if let Some(refresh) = stored.refresh_token {
                        candidates.push(refresh);
                    }
                    if let Some(client_secret) = stored.client_secret {
                        candidates.push(client_secret);
                    }
                }
            }
        }
        candidates.sort();
        candidates.dedup();
        Ok(candidates)
    }
    pub(super) fn config_secret_candidates(config: &McpServerConfig) -> Vec<String> {
        let mut candidates = Vec::new();
        let push_secret = |candidates: &mut Vec<String>, value: &str| {
            let value = value.trim();
            if value.len() >= 8 {
                candidates.push(value.to_string());
            }
        };
        let push_secret_variants = |candidates: &mut Vec<String>, value: &str| {
            push_secret(candidates, value);
            for component in
                value.split(|ch: char| ch.is_ascii_whitespace() || matches!(ch, ',' | ';'))
            {
                let component = component.trim_matches(|ch| matches!(ch, '"' | '\''));
                push_secret(candidates, component);
                if let Some((_, suffix)) = component.split_once('=') {
                    push_secret(
                        candidates,
                        suffix.trim_matches(|ch| matches!(ch, '"' | '\'')),
                    );
                }
            }
        };
        let maybe_push_url_credentials = |candidates: &mut Vec<String>, url: &str| {
            if let Ok(parsed) = url::Url::parse(url) {
                if !parsed.username().is_empty() {
                    push_secret(candidates, parsed.username());
                }
                if let Some(password) = parsed.password() {
                    push_secret(candidates, password);
                }
                for (name, value) in parsed.query_pairs() {
                    let lower_name = name.to_ascii_lowercase();
                    let suspicious_name = [
                        "auth", "token", "key", "secret", "cookie", "session", "sig", "pass",
                        "cred", "bearer",
                    ]
                    .iter()
                    .any(|needle| lower_name.contains(needle));
                    let selector_like = value.len() <= 32
                        && value.bytes().all(|b| {
                            b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_".contains(&b)
                        });
                    if suspicious_name || !selector_like {
                        push_secret(candidates, &value);
                    }
                }
                for segment in parsed.path_segments().into_iter().flatten() {
                    let high_entropy = segment.len() >= 24
                        && segment
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._~+/=%-".contains(&b))
                        && segment.bytes().any(|b| b.is_ascii_alphabetic())
                        && (segment.bytes().any(|b| b.is_ascii_digit())
                            || (segment.bytes().any(|b| b.is_ascii_lowercase())
                                && segment.bytes().any(|b| b.is_ascii_uppercase())));
                    if high_entropy {
                        push_secret(candidates, segment);
                    }
                }
            }
        };
        let maybe_push_headers =
            |candidates: &mut Vec<String>, headers: &lingxi_core::host::McpHeaders| {
                for (name, value) in headers {
                    let lower_name = name.to_ascii_lowercase();
                    let lower_value = value.trim().to_ascii_lowercase();
                    let suspicious_name = [
                        "auth", "token", "key", "secret", "cookie", "session", "sig", "pass",
                        "cred", "bearer",
                    ]
                    .iter()
                    .any(|needle| lower_name.contains(needle));
                    let suspicious_value =
                        lower_value.starts_with("bearer ") || lower_value.starts_with("basic ");
                    let exempt_name = matches!(
                        lower_name.as_str(),
                        "origin" | "referer" | "host" | "user-agent"
                    ) || lower_name.ends_with("-id")
                        || lower_name.ends_with("-version")
                        || lower_name.ends_with("-name");
                    if suspicious_name || suspicious_value || !exempt_name {
                        push_secret_variants(candidates, value);
                    }
                }
            };
        match &config.spec {
            McpTransportSpec::Sse { url, headers, .. }
            | McpTransportSpec::Http { url, headers, .. }
            | McpTransportSpec::WebSocket { url, headers, .. } => {
                maybe_push_url_credentials(&mut candidates, url);
                maybe_push_headers(&mut candidates, headers);
            }
            McpTransportSpec::WsIde {
                url, auth_token, ..
            } => {
                maybe_push_url_credentials(&mut candidates, url);
                if let Some(auth_token) = auth_token {
                    push_secret(&mut candidates, auth_token);
                }
            }
            McpTransportSpec::SseIde {
                url, auth_token, ..
            } => {
                maybe_push_url_credentials(&mut candidates, url);
                if let Some(auth_token) = auth_token {
                    push_secret(&mut candidates, auth_token);
                }
            }
            McpTransportSpec::Stdio { env, .. } => {
                for value in env.values() {
                    push_secret(&mut candidates, value);
                }
            }
            McpTransportSpec::InProcess { .. } | McpTransportSpec::SdkControl { .. } => {}
        }
        candidates
    }
    pub(super) fn discovery_cache_entry_reflects_secret(
        serialized: &str,
        candidates: &[String],
    ) -> bool {
        let contains = |candidate: &str| {
            serialized.contains(candidate)
                || serde_json::to_string(candidate)
                    .ok()
                    .and_then(|escaped| {
                        escaped
                            .strip_prefix('"')
                            .and_then(|s| s.strip_suffix('"'))
                            .map(str::to_string)
                    })
                    .is_some_and(|escaped| serialized.contains(&escaped))
        };
        candidates.iter().any(|candidate| {
            if candidate.is_empty() {
                return false;
            }
            if contains(candidate) {
                return true;
            }
            let encoded: String =
                url::form_urlencoded::byte_serialize(candidate.as_bytes()).collect();
            if encoded == *candidate {
                return false;
            }
            if contains(&encoded) {
                return true;
            }
            let mut lower_percent_hex = encoded.into_bytes();
            let mut index = 0;
            while index + 2 < lower_percent_hex.len() {
                if lower_percent_hex[index] == b'%' {
                    lower_percent_hex[index + 1].make_ascii_lowercase();
                    lower_percent_hex[index + 2].make_ascii_lowercase();
                    index += 3;
                } else {
                    index += 1;
                }
            }
            String::from_utf8(lower_percent_hex)
                .ok()
                .is_some_and(|encoded| contains(&encoded))
        })
    }
    /// §11 — before dialing, compute what the discovery cache decides for
    /// this server (oracle `cot`/`me`). Returns `None` only when no store is
    /// wired (every pre-Stage-2 caller's behavior: never consult, never
    /// serve, never emit). With a store wired this ALWAYS returns `Some`,
    /// including a `Miss` — the caller decides what to do with each variant
    /// (Stage 2: serve `Fresh`/`Stale` without dialing; emit telemetry for a
    /// `Miss` the oracle's `Ko` gate reports, then dial live).
    pub(super) async fn discovery_cache_decision_for(
        &self,
        config: &McpServerConfig,
        negotiation_mode: crate::protocol_negotiation::NegotiationMode,
    ) -> Option<DiscoveryCacheConsult> {
        let store = self.discovery_cache_store.as_ref()?;
        let feature_enabled = crate::discovery_cache::feature_enabled();
        let consult = match crate::discovery_cache::cache_gate_with_metadata(
            &config.spec,
            config.discovery_cache,
            feature_enabled,
            &config.metadata,
        ) {
            Some(reason) => {
                if reason.purges_existing_entry() {
                    let _ = store.purge_server_family(&config.name);
                }
                DiscoveryCacheConsult {
                    decision: crate::discovery_cache::Decision::Miss {
                        reason: reason.miss_reason(),
                    },
                    partition: None,
                }
            }
            None => {
                let partition = match self
                    .discovery_cache_partition_for(config, negotiation_mode)
                    .await
                {
                    Ok(partition) => partition,
                    Err(reason) => {
                        return Some(DiscoveryCacheConsult {
                            decision: crate::discovery_cache::Decision::Miss { reason },
                            partition: None,
                        });
                    }
                };
                let lookup = store.load_partitioned_for_era(
                    &partition.logical_key,
                    &partition.partition_key,
                    partition.expected_era,
                );
                let policy = crate::discovery_cache::DiscoveryCachePolicy::from_env(
                    crate::discovery_cache::now_ms(),
                );
                DiscoveryCacheConsult {
                    decision: crate::discovery_cache::decide_with_metadata(
                        &config.spec,
                        config.discovery_cache,
                        feature_enabled,
                        lookup,
                        policy,
                        &config.metadata,
                    ),
                    partition: Some(partition),
                }
            }
        };
        Some(consult)
    }
    /// §11 Stage 2 — serve a `Fresh`/`Stale` discovery-cache hit WITHOUT
    /// dialing: install a [`McpConnectionState::Cached`] under `key` carrying
    /// the entry's full catalog, emit `tengu_mcp_discovery_source` with
    /// `source` `"cache_fresh"`/`"cache_stale"` and the real `entryAgeMs`
    /// (oracle @182536408's hit branch), and — for a session-level (not
    /// agent-scoped) server — fan the change out on [`Self::catalog_changes`]
    /// exactly like a live connect does, so a mid-session cache hit (a
    /// reconnect that resolves `Fresh`/`Stale` instead of `Miss`) still
    /// reaches the live `ToolRegistry` without a restart. Returns the freshly
    /// allocated [`McpConnectionId`] — see [`McpConnectionState::Cached`]'s
    /// doc for why it is safe to mint one with nothing live behind it.
    pub(super) async fn serve_discovery_cache_hit(
        &self,
        config: &McpServerConfig,
        key: &str,
        entry: crate::discovery_cache::DiscoveryCacheEntry,
        age_ms: u64,
        is_fresh: bool,
        operation_guard: Option<&McpOperationGuard>,
    ) -> Result<McpConnectionId, McpError> {
        if operation_guard.is_some_and(|guard| !guard()) {
            return Err(operation_guard_rejected());
        }
        let connection_id = McpConnectionId::new();
        let negotiated = negotiated_protocol_from_cache_entry(&entry);
        let invalidated_slot = self.invalidate_lazy_upgrade_slot(key).await;
        Self::finish_invalidated_lazy_upgrade_slot(invalidated_slot.as_ref());
        if operation_guard.is_some_and(|guard| !guard()) {
            return Err(operation_guard_rejected());
        }
        self.clear_prompt_predecessors_for_key(key).await;
        let mut connections = self.connections.write().await;
        if operation_guard.is_some_and(|guard| !guard()) {
            return Err(operation_guard_rejected());
        }
        connections.insert(
            key.to_string(),
            McpConnectionState::Cached {
                config: config.clone(),
                connection_id,
                capabilities: entry.capabilities,
                negotiated,
                server_info: entry.server_info,
                tools: entry.tools,
                resources: entry.resources,
                resource_templates: entry.resource_templates,
                prompts: entry.prompts,
                cache_saved_at_ms: entry.saved_at_ms,
                age_ms,
            },
        );
        telemetry::emit_mcp_discovery_source(&telemetry::tengu::mcp::DiscoverySourcePayload {
            transport_type: telemetry::pii::Verified::assert_safe(config.spec.kind().to_string()),
            source: telemetry::pii::Verified::assert_safe(
                if is_fresh {
                    "cache_fresh"
                } else {
                    "cache_stale"
                }
                .to_string(),
            ),
            entry_age_ms: Some(age_ms),
        });
        // Same gate the live-connect tail uses (`table_key.is_none()`):
        // `key == config.name` for an ordinary session-level server (no
        // table-key namespacing applied), `false` for an agent-scoped one
        // (`agent_scope_table_key` never collides with a plain name — see its
        // doc). An agent-scoped cache hit stays private to the subagent that
        // requested it, exactly like a live agent-scoped connect.
        if key == config.name {
            self.publish_catalog_change(McpCatalogChanged {
                server_name: config.name.clone(),
                connection_id,
                retired_connection_id: None,
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            })
            .await;
        }
        Ok(connection_id)
    }
    /// §11 write-through: after a LIVE discovery round completes
    /// (`connect_locked_inner`, right before `config`/`caps`/`tools`/…
    /// move into the `Connected` state), persist the freshly discovered
    /// catalog for a cache-ELIGIBLE server so a future connect can serve it.
    /// The authenticated grant partition is captured before catalog RPCs and
    /// re-resolved before write; a mismatch skips persistence.
    ///
    /// A gate reason [`crate::discovery_cache::CacheGateReason::purges_existing_entry`]
    /// flags instead purges any existing on-disk entry, best-effort — the
    /// write-side counterpart of the same purge the oracle's read-side `cot`
    /// performs on an `opt-out`/`headers-helper` miss.
    ///
    /// Best-effort throughout: any store I/O failure is logged and
    /// swallowed, matching the oracle's `catch(r){Z(e.name, \`Discovery
    /// cache write-through skipped: ${l(r)}\`)}` around `Wo`.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn persist_or_purge_discovery_cache(
        &self,
        config: &McpServerConfig,
        captured_partition: Option<&DiscoveryCachePartition>,
        caps: &ServerCapabilitiesDto,
        tools: &[lingxi_core::host::McpToolDto],
        resources: &[lingxi_core::host::McpResourceDto],
        resource_templates: &[lingxi_core::host::McpResourceTemplateDto],
        prompts: &[lingxi_core::host::McpPromptDto],
        negotiation_mode: crate::protocol_negotiation::NegotiationMode,
        grant_provenance: Option<&GrantProvenance>,
        negotiated: Option<&lingxi_core::host::McpNegotiatedProtocol>,
        metadata: Option<&lingxi_core::host::McpServerMetadataDto>,
    ) {
        let Some(store) = &self.discovery_cache_store else {
            return;
        };
        let feature_enabled = crate::discovery_cache::feature_enabled();
        let gate = crate::discovery_cache::cache_gate_with_metadata(
            &config.spec,
            config.discovery_cache,
            feature_enabled,
            &config.metadata,
        );
        let cache_key = crate::discovery_cache::logical_cache_key(config);
        match gate {
            None => {
                let Some(captured_partition) = captured_partition else {
                    return;
                };
                let Some(captured_grant) = grant_provenance else {
                    return;
                };
                let current_grant = if captured_grant.verify_current {
                    let Ok(Some(current_grant)) = self.current_grant_provenance(config).await
                    else {
                        return;
                    };
                    current_grant
                } else {
                    captured_grant.clone()
                };
                if current_grant != *captured_grant {
                    tracing::warn!(
                        server = %config.name,
                        "Discovery cache write-through skipped because the OAuth grant rotated during discovery"
                    );
                    return;
                }
                let Ok(partition) = self
                    .discovery_cache_partition_for_grant(
                        config,
                        negotiation_mode,
                        Some(&current_grant),
                    )
                    .await
                else {
                    return;
                };
                if &partition != captured_partition {
                    return;
                }
                let mut entry = crate::discovery_cache::DiscoveryCacheEntry::new(
                    cache_key,
                    crate::discovery_cache::now_ms(),
                    caps.clone(),
                    tools.to_vec(),
                    resources.to_vec(),
                    resource_templates.to_vec(),
                    prompts.to_vec(),
                )
                .with_negotiated_era(
                    negotiated
                        .map(|protocol| negotiated_era_label(protocol.era))
                        .unwrap_or("legacy"),
                );
                if let Some(server_info) =
                    metadata.and_then(|metadata| metadata.server_info.as_ref())
                {
                    // The production native caller clips the implementation
                    // before handing it to the disk writer. Never persist its
                    // title/icons/description, instructions or discovery result.
                    let Ok(server_info) = serde_json::from_value::<
                        crate::discovery_cache::DiscoveryCacheServerInfo,
                    >(server_info.clone()) else {
                        return;
                    };
                    entry = entry.with_server_info(server_info);
                }
                let Ok(serialized) = serde_json::to_string(&entry) else {
                    return;
                };
                let Ok(secret_candidates) =
                    self.discovery_cache_secret_candidates_for(config).await
                else {
                    return;
                };
                if Self::discovery_cache_entry_reflects_secret(&serialized, &secret_candidates) {
                    tracing::warn!(
                        server = %config.name,
                        "Discovery cache write-through skipped because the serialized catalog reflected secret material"
                    );
                    return;
                }
                if let Err(error) = store.store_partitioned(&entry, &partition.partition_key) {
                    tracing::warn!(
                        server = %config.name,
                        %error,
                        "Discovery cache write-through skipped"
                    );
                }
            }
            Some(reason) if reason.purges_existing_entry() => {
                if let Err(error) = store.purge_server_family(&config.name) {
                    tracing::warn!(
                        server = %config.name,
                        %error,
                        "Discovery cache purge skipped"
                    );
                }
            }
            Some(_) => {}
        }
    }
    /// Record one Claude 2.1.286 WRt strike against the partition that served
    /// the cached catalog. The caller validates generation and grant ownership.
    /// At the threshold the entry is deleted, so a rebuilt registry cannot
    /// expose the failed catalog again. Ordinary uncached failures never strike.
    pub(super) fn record_discovery_cache_refresh_failure(
        &self,
        config: &McpServerConfig,
        partition: &DiscoveryCachePartition,
    ) {
        let Some(store) = &self.discovery_cache_store else {
            return;
        };
        if let crate::discovery_cache::EntryLookup::Found(mut entry) =
            store.load_partitioned(&partition.logical_key, &partition.partition_key)
        {
            entry.consecutive_refresh_failures =
                entry.consecutive_refresh_failures.saturating_add(1);
            let result = if entry.consecutive_refresh_failures
                >= crate::discovery_cache::strike_threshold()
            {
                store.purge_partitioned(&partition.partition_key)
            } else {
                store.store_partitioned(&entry, &partition.partition_key)
            };
            if let Err(error) = result {
                tracing::warn!(
                    server = %config.name,
                    %error,
                    "Discovery cache strike write skipped"
                );
            }
        }
    }
}
