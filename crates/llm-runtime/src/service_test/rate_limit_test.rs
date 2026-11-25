use super::*;

#[test]
fn concurrent_attempts_keep_their_own_staged_429() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);

    // Stage: a 429 carrying a representative-claim → pending = Some(info).
    let headers = {
        let mut h = std::collections::BTreeMap::new();
        h.insert(
            "anthropic-ratelimit-unified-representative-claim".to_string(),
            "seven_day".to_string(),
        );
        h.insert(
            "anthropic-ratelimit-unified-status".to_string(),
            "rejected".to_string(),
        );
        h
    };
    let older = adapter
        .record_rate_limit_from_429_at_for_route(
            &headers,
            None,
            "claude-sonnet-4-20250514",
            None,
            ProtocolFamily::AnthropicMessages,
            "anthropic",
            10,
        )
        .expect("429 stages its own observation");
    let latest = adapter
        .record_rate_limit_from_429_at_for_route(
            &BTreeMap::new(),
            None,
            "gpt-5",
            None,
            ProtocolFamily::OpenAiResponses,
            "openai",
            20,
        )
        .expect("the second route stages a separate observation");
    adapter.promote_pending_429(Some(older), None);
    assert!(adapter.last_rate_limit_info().is_none());
    adapter.promote_pending_429(Some(latest), None);
    assert_eq!(
        adapter.last_raw_utilization(),
        Some(RawUtilization::default())
    );
}

#[test]
fn prompt_cache_overage_is_scoped_epoch_guarded_and_wait_published() {
    let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    let scope =
        crate::CredentialScope::new(crate::ProviderId::AnthropicFirstParty, "account-profile")
            .with_credential_id("account-credential");
    let prepared = crate::client::PreparedPromptCacheContext {
        scope: scope.clone(),
        account_epoch: 0,
        account_epoch_stale: false,
        is_subscriber: true,
        pending_overage: Arc::new(Mutex::new(None)),
    };
    let header_set = |status: &str, overage_status: &str| {
        BTreeMap::from([
            (
                "anthropic-ratelimit-unified-status".to_owned(),
                status.to_owned(),
            ),
            (
                "anthropic-ratelimit-unified-overage-status".to_owned(),
                overage_status.to_owned(),
            ),
        ])
    };

    let using_overage = header_set("rejected", "allowed");
    adapter.record_prompt_cache_overage_from_headers(&using_overage, Some(&prepared));
    assert!(adapter
        .anthropic
        .prompt_cache_overage
        .is_using_overage(&scope, 0));

    let stale_clear = header_set("allowed", "rejected");
    assert!(!adapter
        .anthropic
        .prompt_cache_overage
        .record(&scope, 0, false, 0));
    assert!(
        adapter
            .anthropic
            .prompt_cache_overage
            .is_using_overage(&scope, 0),
        "older observations cannot clear the current overage state"
    );

    adapter.stage_prompt_cache_overage_from_429(&stale_clear, Some(&prepared));
    assert!(prepared.pending_overage.lock().unwrap().is_some());
    assert!(
        adapter
            .anthropic
            .prompt_cache_overage
            .is_using_overage(&scope, 0),
        "retryable 429 does not change the current state before terminal promotion"
    );
    adapter.promote_pending_429(None, Some(&prepared));
    assert!(!adapter
        .anthropic
        .prompt_cache_overage
        .is_using_overage(&scope, 0));

    let other_scope =
        crate::CredentialScope::new(crate::ProviderId::AnthropicFirstParty, "another-profile");
    assert!(!adapter
        .anthropic
        .prompt_cache_overage
        .is_using_overage(&other_scope, 0));

    let reset = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 30;
    let retry_headers = BTreeMap::from([
        (
            "anthropic-ratelimit-unified-status".to_owned(),
            "rejected".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-overage-status".to_owned(),
            "allowed_warning".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-reset".to_owned(),
            reset.to_string(),
        ),
    ]);
    adapter.record_prompt_cache_overage_from_quota_wait(
        429,
        &retry_headers,
        false,
        Some(&prepared),
    );
    assert!(!adapter
        .anthropic
        .prompt_cache_overage
        .is_using_overage(&scope, 0));
    adapter.record_prompt_cache_overage_from_quota_wait(429, &retry_headers, true, Some(&prepared));
    assert!(adapter
        .anthropic
        .prompt_cache_overage
        .is_using_overage(&scope, 0));

    adapter.account_change_observer().account_changed();
    assert_eq!(adapter.anthropic.prompt_cache_overage.account_epoch(), 1);
    // A response that arrives after the credential owner advanced the
    // account epoch is a late observation for the old request. It must not
    // seed the new account's scope cache.
    adapter.record_prompt_cache_overage_from_headers(&using_overage, Some(&prepared));
    assert!(!adapter.anthropic.prompt_cache_overage.record(
        &scope,
        prepared.account_epoch,
        true,
        u128::MAX
    ));
    assert!(!adapter
        .anthropic
        .prompt_cache_overage
        .is_using_overage(&scope, 1));
}

/// Task 6 (batch 5): a terminal 429 whose response carries the unified
/// headers records the forced-`rejected` snapshot AND the composed
/// limits copy (byte-pinned: no reset header → no ` · resets …` clause,
/// `rateLimitMessages.ts:149` + `:333-344`).
#[tokio::test]
async fn terminal_429_with_unified_headers_records_limits_copy() {
    let transport = FakeTransport::always(ProviderResponse {
        status: 429,
        headers: {
            let mut h = BTreeMap::new();
            h.insert(
                "anthropic-ratelimit-unified-representative-claim".to_string(),
                "seven_day".to_string(),
            );
            h.insert(
                "anthropic-ratelimit-unified-status".to_string(),
                "rejected".to_string(),
            );
            h
        },
        body_json: provider_payload(serde_json::json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "You have reached your usage limit"}
        })),
        request_id: None,
    });
    // Subscriber (non-enterprise) → the 429 is terminal on the first try.
    let adapter = make_adapter_with_subscriber(
        transport.clone(),
        SubscriberState {
            is_subscriber: true,
            is_enterprise: false,
        },
    );
    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;
    assert!(
        matches!(
            classified_error(&result),
            Some(LlmError::RateLimited { .. })
        ),
        "got {result:?}"
    );

    // The composed copy is cached for the orchestrator's terminal re-map.
    assert_eq!(
        adapter.last_rate_limit_error_message().as_deref(),
        Some("You've hit your weekly limit"),
        "seven_day → formatLimitReachedText('weekly limit', '') verbatim"
    );
    // errors.ts:482-516 — the limits snapshot is updated from the error's
    // headers with status FORCED 'rejected'.
    let info = adapter.last_rate_limit_info().expect("snapshot");
    assert_eq!(info.status.as_deref(), Some("rejected"));
    assert_eq!(info.rate_limit_type.as_deref(), Some("seven_day"));
}

/// The `Bha`/`Nha` monotonic guard: an out-of-order (older-timestamp)
/// response must NOT overwrite a newer at-limit snapshot, so the warning
/// cannot flicker off while still at the limit. A genuinely NEWER response
/// still updates.
#[test]
fn stale_parallel_response_does_not_flip_rate_limit_warning_off() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);

    // t=1000: the account is at its weekly limit (rejected).
    adapter.record_rate_limit_from_headers_at(&unified_headers("rejected"), "", 1000);
    assert_eq!(
        adapter.last_rate_limit_info().and_then(|i| i.status),
        Some("rejected".to_string()),
    );

    // t=500: a STALE parallel response reporting 'allowed' arrives late —
    // it must be DROPPED (no flicker): the warning stays 'rejected'.
    adapter.record_rate_limit_from_headers_at(&unified_headers("allowed"), "", 500);
    assert_eq!(
        adapter.last_rate_limit_info().and_then(|i| i.status),
        Some("rejected".to_string()),
        "a stale (older-timestamp) response must not flip the warning off"
    );

    // t=2000: a genuinely NEWER 'allowed' response clears the limit.
    adapter.record_rate_limit_from_headers_at(&unified_headers("allowed"), "", 2000);
    assert_eq!(
        adapter.last_rate_limit_info().and_then(|i| i.status),
        Some("allowed".to_string()),
        "a fresher response must update the snapshot"
    );
}

/// The guard is inert under normal monotonic operation: equal-or-increasing
/// timestamps always record (the production path uses wall-clock `now_ms`).
#[test]
fn equal_or_increasing_timestamps_always_record() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    adapter.record_rate_limit_from_headers_at(&unified_headers("allowed"), "", 5000);
    adapter.record_rate_limit_from_headers_at(&unified_headers("rejected"), "", 5000);
    assert_eq!(
        adapter.last_rate_limit_info().and_then(|i| i.status),
        Some("rejected".to_string()),
        "an equal-timestamp record is not stale and updates"
    );
}

#[test]
fn near_limit_wrap_up_hint_consumes_once_per_window() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    let reset = reset_in(24 * 60 * 60);

    adapter.record_rate_limit_from_headers_at(
        &near_limit_headers("0.95", reset, None),
        "",
        1_000_000,
    );

    assert!(adapter.consume_pending_near_limit_wrap_up_hint());
    assert!(!adapter.consume_pending_near_limit_wrap_up_hint());
}

#[test]
fn near_limit_wrap_up_hint_rearms_only_after_window_changes() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    let first_reset = reset_in(60 * 60);
    let second_reset = reset_in(2 * 60 * 60);

    adapter.record_rate_limit_from_headers_at(
        &near_limit_headers("0.96", first_reset, None),
        "",
        1_000_000,
    );
    assert!(adapter.consume_pending_near_limit_wrap_up_hint());

    adapter.record_rate_limit_from_headers_at(
        &near_limit_headers("0.99", first_reset, None),
        "",
        1_010_000,
    );
    assert!(
        !adapter.consume_pending_near_limit_wrap_up_hint(),
        "same five-hour reset must not re-arm after consumption"
    );

    adapter.record_rate_limit_from_headers_at(
        &near_limit_headers("0.96", second_reset, None),
        "",
        1_020_000,
    );
    assert!(
        adapter.consume_pending_near_limit_wrap_up_hint(),
        "a later five-hour reset opens a new hint window"
    );
}

#[test]
fn stale_parallel_response_does_not_clear_pending_near_limit_wrap_up() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    let reset = reset_in(60 * 60);

    adapter.record_rate_limit_from_headers_at(&near_limit_headers("0.96", reset, None), "", 2_000);
    adapter.record_rate_limit_from_headers_at(
        &near_limit_headers("0.10", reset, Some("allowed_warning")),
        "",
        1_000,
    );

    assert!(
        adapter.consume_pending_near_limit_wrap_up_hint(),
        "stale snapshots must not clear the armed near-limit hint"
    );
}

#[test]
fn near_limit_wrap_up_overage_warning_clears_pending_hint() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    let reset = reset_in(60 * 60);

    adapter.record_rate_limit_from_headers_at(
        &near_limit_headers("0.96", reset, None),
        "",
        1_000_000,
    );
    adapter.record_rate_limit_from_headers_at(
        &near_limit_headers("0.96", reset, Some("allowed_warning")),
        "",
        1_001_000,
    );

    assert!(
        !adapter.consume_pending_near_limit_wrap_up_hint(),
        "allowed_warning must clear the pending near-limit hint"
    );
    adapter.record_rate_limit_from_headers_at(
        &near_limit_headers("0.99", reset, None),
        "",
        1_002_000,
    );
    assert!(
        !adapter.consume_pending_near_limit_wrap_up_hint(),
        "overage clearing preserves the reset key, so the same window cannot re-arm"
    );
}

#[test]
fn near_limit_wrap_up_threshold_uses_max20x_tier() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter_with_subscriber(transport, SubscriberState::default())
        .with_subscription(shared_slot(Some(
            lingxi_core::host::subscription::SubscriptionSnapshot {
                is_subscriber: true,
                subscription_type: Some("max".to_string()),
                rate_limit_tier: Some("default_claude_max_20x".to_string()),
                ..Default::default()
            },
        )));
    let reset = reset_in(60 * 60);

    adapter.record_rate_limit_from_headers_at(
        &near_limit_headers("0.99", reset, None),
        "",
        1_000_000,
    );
    assert!(
        !adapter.consume_pending_near_limit_wrap_up_hint(),
        "max20x must not arm at the looser subscriber threshold"
    );

    adapter.record_rate_limit_from_headers_at(
        &near_limit_headers("0.9975", reset, None),
        "",
        1_001_000,
    );
    assert!(
        adapter.consume_pending_near_limit_wrap_up_hint(),
        "max20x must arm at the stricter 99.75% threshold"
    );
}

#[test]
fn near_limit_wrap_up_uses_future_reset_not_a_five_hour_countdown() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    let now = u64::try_from(ApiService::now_ms() / 1000).unwrap_or(u64::MAX);

    adapter.record_rate_limit_from_headers_at(
        &near_limit_headers("1", now.saturating_sub(1), None),
        "",
        1_000,
    );
    assert!(!adapter.consume_pending_near_limit_wrap_up_hint());

    adapter.record_rate_limit_from_headers_at(
        &near_limit_headers("0.95", now.saturating_add(24 * 60 * 60), None),
        "",
        2_000,
    );
    assert!(
        adapter.consume_pending_near_limit_wrap_up_hint(),
        "the five-hour bucket may reset more than five hours from now"
    );
}

#[test]
fn near_limit_wrap_up_threshold_uses_exact_rate_limit_tier() {
    let reset = reset_in(60 * 60);

    let ordinary_subscriber = make_adapter_with_subscriber(
        FakeTransport::always(ProviderResponse::json(200, ok_response_json())),
        SubscriberState {
            is_subscriber: true,
            is_enterprise: false,
        },
    );
    ordinary_subscriber.record_rate_limit_from_headers_at(
        &near_limit_headers("0.95", reset, None),
        "",
        1_000,
    );
    assert!(
        ordinary_subscriber.consume_pending_near_limit_wrap_up_hint(),
        "subscriber status alone does not select the max-5x threshold"
    );

    let max5 = make_adapter_with_subscriber(
        FakeTransport::always(ProviderResponse::json(200, ok_response_json())),
        SubscriberState::default(),
    )
    .with_subscription(shared_slot(Some(
        lingxi_core::host::subscription::SubscriptionSnapshot {
            rate_limit_tier: Some("default_claude_max_5x".to_string()),
            ..Default::default()
        },
    )));
    max5.record_rate_limit_from_headers_at(&near_limit_headers("0.989", reset, None), "", 1_000);
    assert!(!max5.consume_pending_near_limit_wrap_up_hint());
    max5.record_rate_limit_from_headers_at(&near_limit_headers("0.99", reset, None), "", 2_000);
    assert!(max5.consume_pending_near_limit_wrap_up_hint());
}

/// A 429 carrying BOTH the unified limits headers (gate passes) AND the
/// per-window quartet (raw non-empty) that exhausts the retry budget
/// promotes BOTH: the forced-`rejected` limits snapshot
/// (`last_rate_limit_full`) and the raw per-window utilization
/// (`last_raw_utilization`) — `extractRawUtilization` runs on the SAME
/// error headers (claudeAiLimits.ts:500).
#[tokio::test]
async fn terminal_429_promotes_snapshot_and_raw() {
    let headers = {
        let mut h = BTreeMap::new();
        // Limits gate (from_429_error_headers → Some).
        h.insert(
            "anthropic-ratelimit-unified-representative-claim".to_string(),
            "seven_day".to_string(),
        );
        // Per-window quartet → raw non-empty.
        h.insert(
            "anthropic-ratelimit-unified-5h-utilization".to_string(),
            "0.42".to_string(),
        );
        h.insert(
            "anthropic-ratelimit-unified-5h-reset".to_string(),
            "1750000005".to_string(),
        );
        h.insert(
            "anthropic-ratelimit-unified-7d-utilization".to_string(),
            "0.77".to_string(),
        );
        h.insert(
            "anthropic-ratelimit-unified-7d-reset".to_string(),
            "1750000007".to_string(),
        );
        h
    };
    let transport = FakeTransport::always(ProviderResponse {
        status: 429,
        headers,
        body_json: provider_payload(serde_json::json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "rate limited"}
        })),
        request_id: None,
    });
    // Subscriber (non-enterprise) → the 429 is terminal on the first try.
    let adapter = make_adapter_with_subscriber(
        transport.clone(),
        SubscriberState {
            is_subscriber: true,
            is_enterprise: false,
        },
    );
    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;
    assert!(
        matches!(
            classified_error(&result),
            Some(LlmError::RateLimited { .. })
        ),
        "got {result:?}"
    );

    // Promoted at the terminal: rejected limits snapshot.
    let info = adapter.last_rate_limit_info().expect("snapshot");
    assert_eq!(info.status.as_deref(), Some("rejected"));
    assert_eq!(info.rate_limit_type.as_deref(), Some("seven_day"));

    // Promoted at the terminal: raw per-window utilization from the SAME
    // error headers (extractRawUtilization, ts:500).
    let raw = adapter.last_raw_utilization().expect("raw");
    assert_eq!(raw.five_hour.map(|w| w.utilization), Some(0.42));
    assert_eq!(raw.five_hour.map(|w| w.resets_at), Some(1_750_000_005));
    assert_eq!(raw.seven_day.map(|w| w.utilization), Some(0.77));
    assert_eq!(raw.seven_day.map(|w| w.resets_at), Some(1_750_000_007));
}

/// A terminal 429 with NO per-window quartet must still overwrite the raw
/// snapshot with the EMPTY parse so stale statusline state can be cleared.
#[test]
fn terminal_429_without_raw_headers_clears_raw_snapshot() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);

    adapter.record_rate_limit_from_headers(
        &BTreeMap::from([
            (
                "anthropic-ratelimit-unified-5h-utilization".to_string(),
                "0.42".to_string(),
            ),
            (
                "anthropic-ratelimit-unified-5h-reset".to_string(),
                "1750000005".to_string(),
            ),
        ]),
        "",
    );
    assert_ne!(
        adapter.last_raw_utilization(),
        Some(RawUtilization::default()),
        "precondition: seed a non-empty raw snapshot first"
    );

    let pending =
        adapter.record_rate_limit_from_429(&BTreeMap::new(), None, "claude-sonnet-4-20250514");
    adapter.promote_pending_429(pending, None);

    assert_eq!(
        adapter.last_raw_utilization(),
        Some(RawUtilization::default()),
        "terminal 429 must preserve the EMPTY raw snapshot to clear stale state"
    );
}

/// A 429-with-headers that is RETRIED and then RECOVERS on a 200 must NOT
/// leave the rejected snapshot behind. This verifies the SUCCESS PATH:
/// after recovery, `last_rate_limit` reflects the 200 (here headerless →
/// `None`), NOT the retried 429 — because the limits cache is only written
/// at terminal promotion, which never fires on a recovered turn.
#[tokio::test]
async fn retried_429_does_not_update_limits_snapshot() {
    let headers_429 = {
        let mut h = BTreeMap::new();
        h.insert(
            "anthropic-ratelimit-unified-representative-claim".to_string(),
            "seven_day".to_string(),
        );
        h.insert(
            "anthropic-ratelimit-unified-status".to_string(),
            "rejected".to_string(),
        );
        // retry-after 0 → no real sleep.
        h.insert("retry-after".to_string(), "0".to_string());
        h
    };
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse {
            status: 429,
            headers: headers_429,
            body_json: provider_payload(serde_json::json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "rate limited"}
            })),
            request_id: None,
        }),
        // Recovery: a 200 with NO unified headers.
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    // Enterprise subscriber → the 429 is retried, then the 200 recovers.
    let adapter = make_adapter_with_subscriber(
        transport.clone(),
        SubscriberState {
            is_subscriber: true,
            is_enterprise: true,
        },
    );
    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;
    assert!(result.is_ok(), "429 then 200 must recover: {result:?}");

    // The success seam cleared the limits cache (the 200 carried no unified
    // headers); the retried 429's rejected snapshot was NEVER promoted.
    assert_eq!(
        adapter.last_rate_limit_info(),
        None,
        "retried-then-recovered 429 must not plant a rejected snapshot"
    );
    // The 429 message copy was likewise cleared on success.
    assert_eq!(adapter.last_rate_limit_error_message(), None,);
}

/// Cross-drive isolation, asserted on the OBSERVABLE promoted snapshot: a
/// pending 429 staged by an EARLIER drive must never survive into a LATER
/// drive's terminal promote. Drive A = `[429-with-headers(seven_day),
/// 400-invalid-request terminal]` — A stages a `seven_day` snapshot but the
/// 400 is non-RateLimited, so it never promotes and the slot is orphaned.
/// Drive B ends on a TERMINAL 429 carrying its OWN fresh headers
/// (`five_hour`, DIFFERENT from A's) → B promotes B's snapshot. The promoted
/// `last_rate_limit_full()` must read `five_hour` (DRIVE B), proving A's
/// orphaned `seven_day` slot did NOT leak in.
///
/// Each response supplies its own limits to terminal promotion.
#[tokio::test]
async fn stale_pending_429_not_promoted_across_drives() {
    // Drive A's 429: representative-claim = seven_day. Staged then orphaned.
    let resp_429_a = ProviderResponse {
        status: 429,
        headers: {
            let mut h = BTreeMap::new();
            h.insert(
                "anthropic-ratelimit-unified-representative-claim".to_string(),
                "seven_day".to_string(),
            );
            h.insert(
                "anthropic-ratelimit-unified-status".to_string(),
                "rejected".to_string(),
            );
            h.insert("retry-after".to_string(), "0".to_string());
            h
        },
        body_json: provider_payload(serde_json::json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "rate limited"}
        })),
        request_id: None,
    };
    // A plain 400 invalid_request → terminal, NON-rate-limited (no promote).
    let resp_400 = ProviderResponse {
        status: 400,
        headers: BTreeMap::new(),
        body_json: provider_payload(serde_json::json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": "bad request"}
        })),
        request_id: None,
    };
    // Drive B's 429: representative-claim = five_hour (DIFFERENT from A).
    // Terminal here → B promotes B's OWN fresh snapshot.
    let resp_429_b = ProviderResponse {
        status: 429,
        headers: {
            let mut h = BTreeMap::new();
            h.insert(
                "anthropic-ratelimit-unified-representative-claim".to_string(),
                "five_hour".to_string(),
            );
            h.insert(
                "anthropic-ratelimit-unified-status".to_string(),
                "rejected".to_string(),
            );
            h.insert("retry-after".to_string(), "0".to_string());
            h
        },
        body_json: provider_payload(serde_json::json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "rate limited"}
        })),
        request_id: None,
    };
    // Drive A consumes idx 0,1; Drive B consumes idx 2,3 (global cursor).
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(resp_429_a),
        FakeResponse::Ok(resp_400),
        FakeResponse::Ok(resp_429_b.clone()),
        FakeResponse::Ok(resp_429_b),
    ]);
    // Non-subscriber (429 is retryable), settings_max_retries=1 so each
    // drive retries exactly once then terminates; backoff 0 → no sleeps.
    let adapter = make_adapter_with_routing(
        transport.clone(),
        std::collections::BTreeMap::new(),
        Some(1),
        Some(0),
    );

    // Drive A: 429(seven_day) (retried, pending set) → 400 (terminal,
    // non-RateLimited → no promotion). Pending lingers with A's snapshot.
    let a = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-haiku-4-20250307",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;
    assert!(
        matches!(classified_error(&a), Some(LlmError::InvalidRequest { .. })),
        "drive A must die on the 400, got {a:?}"
    );

    // Drive B: 429(five_hour) (retried, pending OVERWRITTEN with B's
    // snapshot) → 429(five_hour) terminal → promotes B's snapshot.
    let b = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-haiku-4-20250307",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;
    assert!(
        matches!(classified_error(&b), Some(LlmError::RateLimited { .. })),
        "drive B must die on its terminal 429, got {b:?}"
    );

    // The promoted snapshot must be DRIVE B's (five_hour), proving drive A's
    // orphaned seven_day slot did not survive into B's promote.
    let info = adapter
        .last_rate_limit_info()
        .expect("drive B promotes its own snapshot");
    assert_eq!(
        info.rate_limit_type.as_deref(),
        Some("five_hour"),
        "promoted snapshot must reflect DRIVE B (five_hour), not A's stale seven_day"
    );
    assert_eq!(info.status.as_deref(), Some("rejected"));
}

/// Task 6 (batch 5): a 429 WITHOUT unified headers fails the
/// `if (rateLimitType || overageStatus)` gate (errors.ts:480) — no copy
/// is composed, and a copy from an earlier 429 is superseded (the slot
/// reflects the most recent 429), so the generic surface applies.
#[tokio::test]
async fn terminal_429_without_unified_headers_records_no_copy() {
    let transport = FakeTransport::always(ProviderResponse {
        status: 429,
        headers: BTreeMap::new(),
        body_json: provider_payload(serde_json::json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "rate limited"}
        })),
        request_id: None,
    });
    let adapter = make_adapter_with_subscriber(
        transport.clone(),
        SubscriberState {
            is_subscriber: true,
            is_enterprise: false,
        },
    );
    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;
    assert!(result.is_err());
    assert_eq!(
        adapter.last_rate_limit_error_message(),
        None,
        "no unified headers on the 429 → no limits copy"
    );
}

/// Task 6 (batch 5): the live subscription slot's `pro` plan flips the
/// `seven_day_sonnet` wording to "weekly limit"
/// (`rateLimitMessages.ts:176-181`).
#[tokio::test]
async fn terminal_429_sonnet_copy_uses_pro_subscription_wording() {
    let resp_429 = |claim: &str| ProviderResponse {
        status: 429,
        headers: {
            let mut h = BTreeMap::new();
            h.insert(
                "anthropic-ratelimit-unified-representative-claim".to_string(),
                claim.to_string(),
            );
            h
        },
        body_json: provider_payload(serde_json::json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "rate limited"}
        })),
        request_id: None,
    };

    // Without a pro/enterprise snapshot → "Sonnet limit".
    let adapter = make_adapter_with_subscriber(
        FakeTransport::always(resp_429("seven_day_sonnet")),
        SubscriberState {
            is_subscriber: true,
            is_enterprise: false,
        },
    );
    let _ = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;
    assert_eq!(
        adapter.last_rate_limit_error_message().as_deref(),
        Some("You've hit your Sonnet limit")
    );

    // With a live `pro` snapshot → "weekly limit".
    let pro = make_adapter_with_subscriber(
        FakeTransport::always(resp_429("seven_day_sonnet")),
        SubscriberState {
            is_subscriber: true,
            is_enterprise: false,
        },
    )
    .with_subscription(shared_slot(Some(
        lingxi_core::host::subscription::SubscriptionSnapshot {
            is_subscriber: true,
            subscription_type: Some("pro".to_string()),
            ..Default::default()
        },
    )));
    let _ = pro
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;
    assert_eq!(
        pro.last_rate_limit_error_message().as_deref(),
        Some("You've hit your weekly limit")
    );
}

/// Task 8 gate: enterprise subscriber 429 → retries through the full budget.
///
/// Wire: `SubscriberState { is_subscriber: true, is_enterprise: true }` +
/// a transport that returns 429 then 200 → the adapter retries and succeeds.
#[tokio::test]
async fn enterprise_subscriber_429_retries_in_adapter() {
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse {
            status: 429,
            headers: {
                let mut h = BTreeMap::new();
                h.insert("retry-after".to_string(), "0".to_string());
                h
            },
            body_json: provider_payload(serde_json::json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "rate limited"}
            })),
            request_id: None,
        }),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let adapter = make_adapter_with_subscriber(
        transport.clone(),
        SubscriberState {
            is_subscriber: true,
            is_enterprise: true,
        },
    );
    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;
    assert!(
        result.is_ok(),
        "enterprise subscriber 429 must retry and succeed"
    );
    assert_eq!(
        transport.seen_count(),
        2,
        "should have made 2 requests (429 then 200)"
    );
}

/// #5 (main-loop parity): the adapter surfaces the most recent drive's
/// budget-consuming retry count via `last_retry_count()`, so the cost path
/// can record the REAL retry count (not the previous hardcoded `0`). A 429
/// followed by a 200 is exactly one retry.
#[tokio::test]
async fn last_retry_count_reflects_429_retry() {
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse {
            status: 429,
            headers: {
                let mut h = BTreeMap::new();
                h.insert("retry-after".to_string(), "0".to_string());
                h
            },
            body_json: provider_payload(serde_json::json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "rate limited"}
            })),
            request_id: None,
        }),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let adapter = make_adapter_with_subscriber(
        transport.clone(),
        SubscriberState {
            is_subscriber: true,
            is_enterprise: true,
        },
    );
    // No drive yet ⇒ zero.
    assert_eq!(adapter.last_retry_count(), 0);
    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;
    assert!(result.is_ok(), "429→200 must succeed");
    // Exactly one budget-consuming retry was performed.
    assert_eq!(
        adapter.last_retry_count(),
        1,
        "one 429 retry ⇒ last_retry_count() == 1"
    );
}
