use super::*;

/// Per-model fallback entry wins over global fallback_model.
///
/// Uses `claude-opus-4-6` as primary (is_non_custom_opus = true → allow_fallback
/// activates naturally for a non-subscriber, no process env mutation needed).
/// After 3 consecutive 529s the per-model fallback to haiku fires.
#[tokio::test]
async fn per_model_fallback_wins_over_global() {
    let haiku_ok = serde_json::json!({
        "id": "msg_haiku",
        "model": "claude-haiku-4-20250307",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 2}
    });
    let overloaded_body = serde_json::json!({
        "type": "error",
        "error": {"type": "overloaded_error", "message": "Overloaded"}
    });
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(529, overloaded_body.clone())),
        FakeResponse::Ok(ProviderResponse::json(529, overloaded_body.clone())),
        FakeResponse::Ok(ProviderResponse::json(529, overloaded_body.clone())),
        FakeResponse::Ok(ProviderResponse::json(200, haiku_ok)),
    ]);

    let mut fallback_overrides = std::collections::BTreeMap::new();
    // Per-model: opus → haiku (single-entry chain).
    fallback_overrides.insert(
        "claude-opus-4-6".to_string(),
        vec!["claude-haiku-4-20250307".to_string()],
    );

    // No env var needed: claude-opus-4-6 is_non_custom_opus=true → allow_fallback=true
    // for non-subscriber (default SubscriberState).
    let mut adapter =
        make_adapter_with_routing(transport.clone(), fallback_overrides, None, Some(0));
    // Global fallback also points somewhere — per-model must win.
    adapter.fallback_models = vec!["claude-sonnet-4-20250514".to_string()];

    let result = adapter
        .messages_create({
            let mut request = crate::MessagesCreateRequest::new(
                "claude-opus-4-6",
                None,
                None,
                Vec::new(),
                Vec::new(),
            );
            request.opts.fallback = match None {
                Some(models) => crate::FallbackPolicy::from_models_csv(models),
                None => crate::FallbackPolicy::Configured,
            };
            request
        })
        .await;

    // Should succeed — 3 × 529 then haiku 200.
    assert!(
        result.is_ok(),
        "per-model fallback should route to haiku and succeed: {result:?}"
    );
    assert_eq!(
        transport.seen_count(),
        4,
        "expected 4 requests: 3 × 529 + 1 × 200"
    );
}

/// Global fallback is used when no per-model entry is present.
#[tokio::test]
async fn global_fallback_used_when_no_per_model_entry() {
    let haiku_ok = serde_json::json!({
        "id": "msg_haiku2",
        "model": "claude-haiku-4-20250307",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 2}
    });
    let overloaded_body = serde_json::json!({
        "type": "error",
        "error": {"type": "overloaded_error", "message": "Overloaded"}
    });
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(529, overloaded_body.clone())),
        FakeResponse::Ok(ProviderResponse::json(529, overloaded_body.clone())),
        FakeResponse::Ok(ProviderResponse::json(529, overloaded_body.clone())),
        FakeResponse::Ok(ProviderResponse::json(200, haiku_ok)),
    ]);

    // No per-model overrides.
    let mut adapter = make_adapter_with_routing(
        transport.clone(),
        std::collections::BTreeMap::new(),
        None,
        Some(0),
    );
    // Global fallback: opus → haiku.
    adapter.fallback_models = vec!["claude-haiku-4-20250307".to_string()];

    // claude-opus-4-6 is_non_custom_opus=true → allow_fallback=true for non-subscriber.
    let result = adapter
        .messages_create({
            let mut request = crate::MessagesCreateRequest::new(
                "claude-opus-4-6",
                None,
                None,
                Vec::new(),
                Vec::new(),
            );
            request.opts.fallback = match None {
                Some(models) => crate::FallbackPolicy::from_models_csv(models),
                None => crate::FallbackPolicy::Configured,
            };
            request
        })
        .await;

    assert!(result.is_ok(), "global fallback should work: {result:?}");
    assert_eq!(transport.seen_count(), 4);
}

/// Per-model fallback fires even when the request uses an ALIAS of the
/// primary model.
///
/// `fallback_overrides` keys are keyed by the display model; if the request
/// arrives as an alias (e.g. `"claude"` instead of `"claude-sonnet-4-20250514"`)
/// the lookup must normalize via `alias_to_display` before probing the map.
///
/// RED on the old code (raw model probe skips the per-model entry when an
/// alias is used); GREEN after the alias normalization fix.
#[tokio::test]
async fn per_model_fallback_fires_via_alias() {
    // Three 529s then a success on the fallback (haiku).
    let overloaded_json = serde_json::json!({
        "type": "error",
        "error": {"type": "overloaded_error", "message": "overloaded"}
    });
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(529, overloaded_json.clone())),
        FakeResponse::Ok(ProviderResponse::json(529, overloaded_json.clone())),
        FakeResponse::Ok(ProviderResponse::json(529, overloaded_json.clone())),
        FakeResponse::Ok(ProviderResponse::json(200, routing_ok_response_json())),
    ]);

    // Per-model fallback: display "claude-sonnet-4-20250514" → "claude-haiku-4-20250307"
    // (single-entry chain).
    let mut fallback_overrides = std::collections::BTreeMap::new();
    fallback_overrides.insert(
        "claude-sonnet-4-20250514".to_string(),
        vec!["claude-haiku-4-20250307".to_string()],
    );

    let adapter = make_adapter_with_routing(transport.clone(), fallback_overrides, None, Some(0));

    // Request via ALIAS — the alias_to_display map must normalize this to
    // "claude-sonnet-4-20250514" before the fallback_overrides lookup.
    let result = adapter
        .messages_create({
            let mut request = crate::MessagesCreateRequest::new(
                "claude",
                // alias of "claude-sonnet-4-20250514"
                None,
                custom_system_prompt(Some("sys")),
                Vec::new(),
                Vec::new(),
            );
            request.opts.fallback = match None {
                Some(models) => crate::FallbackPolicy::from_models_csv(models),
                None => crate::FallbackPolicy::Configured,
            };
            request
        })
        .await;

    assert!(
        result.is_ok(),
        "alias-keyed per-model fallback should fire and succeed; got: {result:?}"
    );
    // 3 primary 529s + 1 fallback success = 4 transport calls.
    assert_eq!(
        transport.seen_count(),
        4,
        "expected 3 failing primary calls + 1 successful fallback call"
    );
}

#[test]
fn fallback_csv_is_ordered_trimmed_and_deduplicated() {
    assert_eq!(
        crate::service::request::parse_fallback_chain(" sonnet,haiku,sonnet "),
        vec!["sonnet".to_string(), "haiku".to_string()]
    );
}

/// 2-entry chain: primary → chain[0] → chain[1] when all 529s.
///
/// Scripted: 3×529 on primary, 3×529 on chain[0], then 200 on chain[1].
/// Asserts the model sequence primary→c0→c1 via captured request bodies.
#[tokio::test]
async fn two_entry_chain_walks_both_entries() {
    // The drive loop's attempt budget is only stable while the dispatch
    // opt-in stays OFF: a leaked `tengu_cedar_lattice` adds one extra,
    // budget-free "strip and retry" attempt on the first 5xx.
    let _dispatch_off = cedar_lattice_read();
    let overloaded = serde_json::json!({
        "type": "error",
        "error": {"type": "overloaded_error", "message": "Overloaded"}
    });
    let c1_ok = serde_json::json!({
        "id": "msg_c1",
        "model": "claude-haiku-4-20250307",
        "content": [{"type": "text", "text": "c1 ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 2}
    });
    // 3 × 529 on primary (claude-opus-4-6)
    // 3 × 529 on chain[0] (claude-sonnet-4-20250514)
    // 1 × 200 on chain[1] (claude-haiku-4-20250307)
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
        FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
        FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
        FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
        FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
        FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
        FakeResponse::Ok(ProviderResponse::json(200, c1_ok)),
    ]);

    let mut fallback_overrides = std::collections::BTreeMap::new();
    // 2-entry chain: opus-4-6 → sonnet-4-20250514 → haiku-4-20250307
    fallback_overrides.insert(
        "claude-opus-4-6".to_string(),
        vec![
            "claude-sonnet-4-20250514".to_string(),
            "claude-haiku-4-20250307".to_string(),
        ],
    );

    let adapter = make_adapter_with_routing(transport.clone(), fallback_overrides, None, Some(0));

    let result = adapter
        .messages_create({
            let mut request = crate::MessagesCreateRequest::new(
                "claude-opus-4-6",
                None,
                None,
                Vec::new(),
                Vec::new(),
            );
            request.opts.fallback = match None {
                Some(models) => crate::FallbackPolicy::from_models_csv(models),
                None => crate::FallbackPolicy::Configured,
            };
            request
        })
        .await;

    assert!(
        result.is_ok(),
        "chain walk must succeed on chain[1]: {result:?}"
    );
    assert_eq!(
        transport.seen_count(),
        7,
        "3 primary + 3 chain[0] + 1 chain[1]"
    );

    // Assert the model sequence: first 3 requests use primary, next 3 use chain[0],
    // last 1 uses chain[1].
    let primary = "claude-opus-4-6";
    let c0 = "claude-sonnet-4-20250514";
    let c1 = "claude-haiku-4-20250307";
    for i in 0..3 {
        assert_eq!(
            transport.seen_body_model(i).as_deref(),
            Some(primary),
            "request {i} must use primary model"
        );
    }
    for i in 3..6 {
        assert_eq!(
            transport.seen_body_model(i).as_deref(),
            Some(c0),
            "request {i} must use chain[0]"
        );
    }
    assert_eq!(
        transport.seen_body_model(6).as_deref(),
        Some(c1),
        "request 6 must use chain[1]"
    );
}

/// Chain exhaustion: when all entries are overloaded, the call is terminal.
///
/// Single-entry chain: primary 3×529 → chain[0] persistent 529 → terminal error.
#[tokio::test]
async fn chain_exhausted_gives_terminal_error() {
    let overloaded = serde_json::json!({
        "type": "error",
        "error": {"type": "overloaded_error", "message": "Overloaded"}
    });
    // primary: 3 × 529 → Fallback
    // chain[0]: persistent 529 → RepeatedOverloaded (is_external=true in make_adapter_with_routing)
    let transport = FakeTransport::always(ProviderResponse::json(529, overloaded));

    let mut fallback_overrides = std::collections::BTreeMap::new();
    fallback_overrides.insert(
        "claude-opus-4-6".to_string(),
        vec!["claude-haiku-4-20250307".to_string()],
    );

    let adapter = make_adapter_with_routing(transport.clone(), fallback_overrides, None, Some(0));

    let result = adapter
        .messages_create({
            let mut request = crate::MessagesCreateRequest::new(
                "claude-opus-4-6",
                None,
                None,
                Vec::new(),
                Vec::new(),
            );
            request.opts.fallback = match None {
                Some(models) => crate::FallbackPolicy::from_models_csv(models),
                None => crate::FallbackPolicy::Configured,
            };
            request
        })
        .await;

    assert!(
        result.is_err(),
        "exhausted chain must produce terminal error"
    );
    // The error must be Overloaded (either repeated=true from external path or
    // plain Overloaded — either variant indicates the chain was walked and terminated).
    assert!(
        matches!(classified_error(&result), Some(LlmError::Overloaded { .. })),
        "terminal error must be LlmError::Overloaded"
    );
}

/// Single-entry chain behaves like batch-1 (exactly one fallback hop).
///
/// Uses the existing `per_model_fallback_wins_over_global` scenario but
/// verifies via `seen_body_model` that the model sequence is correct.
#[tokio::test]
async fn single_entry_chain_behaves_like_batch1() {
    // The drive loop's attempt budget is only stable while the dispatch
    // opt-in stays OFF: a leaked `tengu_cedar_lattice` adds one extra,
    // budget-free "strip and retry" attempt on the first 5xx.
    let _dispatch_off = cedar_lattice_read();
    let overloaded = serde_json::json!({
        "type": "error",
        "error": {"type": "overloaded_error", "message": "Overloaded"}
    });
    let haiku_ok = serde_json::json!({
        "id": "msg_haiku",
        "model": "claude-haiku-4-20250307",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 2}
    });
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
        FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
        FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
        FakeResponse::Ok(ProviderResponse::json(200, haiku_ok)),
    ]);

    let mut fallback_overrides = std::collections::BTreeMap::new();
    fallback_overrides.insert(
        "claude-opus-4-6".to_string(),
        vec!["claude-haiku-4-20250307".to_string()],
    );
    let adapter = make_adapter_with_routing(transport.clone(), fallback_overrides, None, Some(0));

    let result = adapter
        .messages_create({
            let mut request = crate::MessagesCreateRequest::new(
                "claude-opus-4-6",
                None,
                None,
                Vec::new(),
                Vec::new(),
            );
            request.opts.fallback = match None {
                Some(models) => crate::FallbackPolicy::from_models_csv(models),
                None => crate::FallbackPolicy::Configured,
            };
            request
        })
        .await;

    assert!(
        result.is_ok(),
        "single-entry chain must succeed: {result:?}"
    );
    assert_eq!(transport.seen_count(), 4, "3 primary 529s + 1 fallback 200");
    // First 3 requests: primary model.
    for i in 0..3 {
        assert_eq!(
            transport.seen_body_model(i).as_deref(),
            Some("claude-opus-4-6"),
            "request {i} must use primary"
        );
    }
    // 4th request: fallback model.
    assert_eq!(
        transport.seen_body_model(3).as_deref(),
        Some("claude-haiku-4-20250307"),
        "request 3 must use chain[0]"
    );
}

/// Global fallback_model still works when no per-model chain is configured.
///
/// The global `fallback_model` is wrapped into a single-entry chain and walks
/// the same code path; this test guards that wiring.
#[tokio::test]
async fn global_fallback_model_works_without_chain_entry() {
    // The drive loop's attempt budget is only stable while the dispatch
    // opt-in stays OFF: a leaked `tengu_cedar_lattice` adds one extra,
    // budget-free "strip and retry" attempt on the first 5xx.
    let _dispatch_off = cedar_lattice_read();
    let overloaded = serde_json::json!({
        "type": "error",
        "error": {"type": "overloaded_error", "message": "Overloaded"}
    });
    let sonnet_ok = serde_json::json!({
        "id": "msg_sonnet",
        "model": "claude-sonnet-4-20250514",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 2}
    });
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
        FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
        FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
        FakeResponse::Ok(ProviderResponse::json(200, sonnet_ok)),
    ]);

    let mut adapter = make_adapter_with_routing(
        transport.clone(),
        std::collections::BTreeMap::new(),
        None,
        Some(0),
    );
    // Set global fallback only (no per-model chain).
    adapter.fallback_models = vec!["claude-sonnet-4-20250514".to_string()];

    let result = adapter
        .messages_create({
            let mut request = crate::MessagesCreateRequest::new(
                "claude-opus-4-6",
                None,
                None,
                Vec::new(),
                Vec::new(),
            );
            request.opts.fallback = match None {
                Some(models) => crate::FallbackPolicy::from_models_csv(models),
                None => crate::FallbackPolicy::Configured,
            };
            request
        })
        .await;

    assert!(result.is_ok(), "global fallback must work: {result:?}");
    assert_eq!(transport.seen_count(), 4);
    // Request 3 must use the global fallback model.
    assert_eq!(
        transport.seen_body_model(3).as_deref(),
        Some("claude-sonnet-4-20250514"),
        "request 3 must use global fallback model"
    );
}

/// `settings_max_retries=2` causes terminal after 3 executions (not 11).
///
/// The adapter reads `LINGXI_MAX_RETRIES` from the process env in
/// `messages_create`.  To avoid interference with parallel tests we verify
/// via the retry.rs layer (which is injected, not process-env) rather than
/// through the adapter's env path.  The adapter's `settings_max_retries`
/// field is directly observable via the resolve_retry_control_with_settings
/// call: when env var is absent it uses `settings_max_retries` as the
/// effective limit.  We temporarily clear the env var then restore it.
#[tokio::test]
async fn settings_max_retries_beats_default() {
    // The drive loop's attempt budget is only stable while the dispatch
    // opt-in stays OFF: a leaked `tengu_cedar_lattice` adds one extra,
    // budget-free "strip and retry" attempt on the first 5xx.
    let _dispatch_off = cedar_lattice_read();
    // We can test the settings path directly: when the env var is absent
    // the settings_max_retries field applies.  We control LINGXI_MAX_RETRIES
    // for the duration of this test — accept minor isolation risk since the
    // pre-existing test suite also does this.
    let transport = FakeTransport::sequence(vec![FakeResponse::Ok(ProviderResponse::json(
        500,
        serde_json::json!({"type": "error", "error": {"type": "api_error", "message": "internal"}}),
    ))]);

    let adapter = make_adapter_with_routing(
        transport.clone(),
        std::collections::BTreeMap::new(),
        Some(2), // settings says max 2 retries
        None,
    );

    // Temporarily unset LINGXI_MAX_RETRIES so settings value wins.
    let saved = std::env::var("LINGXI_MAX_RETRIES").ok();
    #[allow(deprecated)]
    std::env::remove_var("LINGXI_MAX_RETRIES");

    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;

    // Restore.
    if let Some(v) = saved {
        #[allow(deprecated)]
        std::env::set_var("LINGXI_MAX_RETRIES", v);
    }

    assert!(result.is_err(), "must fail after exhausting budget");
    // max_retries=2 → 3 executions (2 sleeps + 1 final).
    assert_eq!(
        transport.seen_count(),
        3,
        "settings_max_retries=2 should give 3 executions (2 retries + 1 initial)"
    );
}

/// Env `LINGXI_MAX_RETRIES` beats `settings_max_retries`.
///
/// Proven via `resolve_retry_control_with_settings` unit tests in retry.rs;
/// this adapter-level test verifies the wiring by injecting through
/// `ResolveRetryEnv` directly rather than mutating the process env.
///
/// We use `crate::model::retry::resolve_retry_control_with_settings` to
/// build the expected `RetryControl` and compare `max_retries`.
#[test]
fn env_max_retries_beats_settings_via_resolve() {
    use crate::model::retry::{
        resolve_retry_control_with_settings, ResolveRetryEnv, DEFAULT_MAX_RETRIES,
    };

    // env=Some("1") + settings=Some(8) → max_retries=1 (env wins).
    let env_with_1 = ResolveRetryEnv {
        max_retries: Some("1".to_string()),
        ..ResolveRetryEnv::default()
    };
    let ctl = resolve_retry_control_with_settings(
        "claude-sonnet-4-20250514",
        None,
        false,
        &env_with_1,
        Some(8),
    );
    assert_eq!(ctl.max_retries, 1, "env(1) must beat settings(8)");

    // env=None + settings=Some(7) → max_retries=7 (settings wins).
    let env_absent = ResolveRetryEnv::default();
    let ctl2 = resolve_retry_control_with_settings(
        "claude-sonnet-4-20250514",
        None,
        false,
        &env_absent,
        Some(7),
    );
    assert_eq!(ctl2.max_retries, 7, "settings(7) must beat default(10)");

    // env=None + settings=None → DEFAULT.
    let ctl3 = resolve_retry_control_with_settings(
        "claude-sonnet-4-20250514",
        None,
        false,
        &env_absent,
        None,
    );
    assert_eq!(ctl3.max_retries, DEFAULT_MAX_RETRIES);
}

/// `settings_backoff_ms=1000` doubles the jitter ladder base.
///
/// With `backoff_ms=1000` and time paused, we verify the first retry delay
/// is ≥ 800 ms (= 1000 × 0.8 lower-jitter-bound).  Without the setting the
/// base would be 500 ms (lower bound 400 ms) — so 800 ms is above the
/// un-scaled upper bound (600 ms) which proves scaling is active.
#[tokio::test(start_paused = true)]
async fn backoff_ms_scales_jitter_base() {
    // The drive loop's attempt budget is only stable while the dispatch
    // opt-in stays OFF: a leaked `tengu_cedar_lattice` adds one extra,
    // budget-free "strip and retry" attempt on the first 5xx.
    let _dispatch_off = cedar_lattice_read();
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(
            500,
            serde_json::json!({"type": "error", "error": {"type": "api_error", "message": "err"}}),
        )),
        FakeResponse::Ok(ProviderResponse::json(200, routing_ok_response_json())),
    ]);

    let adapter = make_adapter_with_routing(
        transport.clone(),
        std::collections::BTreeMap::new(),
        None,
        Some(1000), // backoff_ms = 1000 → first rung 1000, additive jitter [1000, 1250)
    );

    let before = tokio::time::Instant::now();
    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;
    let elapsed = before.elapsed();

    assert!(result.is_ok(), "should succeed after retry: {result:?}");
    // Additive jitter (binary `sle`: base + rand(0,0.25)·base) → base 1000
    // gives [1000, 1250). Default base 500 → [500, 625), upper 625 < 800.
    // So ≥ 800 ms proves the 1000 ms base (not the default 500) is in effect.
    assert!(
        elapsed >= std::time::Duration::from_millis(800),
        "backoff_ms=1000 should produce ≥ 800 ms delay; elapsed={elapsed:?}"
    );
    // Must be < 1250 ms (upper additive-jitter bound: 1000 + 0.25·1000).
    assert!(
        elapsed < std::time::Duration::from_millis(1250),
        "backoff_ms=1000 delay should be < 1250 ms; elapsed={elapsed:?}"
    );
}

/// A 2xx response with `anthropic-ratelimit-unified-overage-status: rejected`
/// must be stored in `last_rate_limit_info` and trigger a `tracing::warn!`.
///
/// We only assert the data is stored; the warn fires on a live-log subscriber
/// which we do not attach in tests — the absence of a panic is the assertion.
#[tokio::test]
async fn rate_limit_info_stored_from_2xx_response() {
    let mut headers = BTreeMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-representative-claim".to_string(),
        "five_hour".to_string(),
    );
    headers.insert(
        "anthropic-ratelimit-unified-overage-status".to_string(),
        "rejected".to_string(),
    );

    let transport = FakeTransport::always(ProviderResponse {
        status: 200,
        headers,
        body_json: provider_payload(ok_response_json()),
        request_id: None,
    });
    let adapter = make_adapter(transport);
    let _ = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("ok");

    let info = adapter
        .last_rate_limit_info()
        .expect("last_rate_limit_info must be Some after a 2xx with unified headers");
    assert_eq!(
        info.rate_limit_type.as_deref(),
        Some("five_hour"),
        "rate_limit_type must be five_hour"
    );
    assert_eq!(
        info.overage_status.as_deref(),
        Some("rejected"),
        "overage_status must be rejected"
    );
}

#[tokio::test]
async fn aws_auth_error_refreshes_and_retries_once() {
    // 401 (expired STS) then 200: the hook must run ZBd once and the retry
    // must succeed — the 2.1.198 behavior replacing the "/login" dead end.
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(expired_sts_response()),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let aws = Arc::new(CountingAwsRefresh::default());
    let adapter = make_bedrock_adapter_with_aws(transport.clone(), aws.clone());

    let mut request =
        crate::MessagesCreateRequest::new("model", None, None, Vec::new(), Vec::new());
    request.opts.max_output_tokens = Some(4096);
    let out = adapter.messages_create(request).await;
    assert!(out.is_ok(), "retry after refresh must succeed: {out:?}");
    assert_eq!(
        aws.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "exactly one refresh"
    );
    assert_eq!(transport.seen_count(), 2, "original + one retry");
}

#[tokio::test]
async fn aws_auth_retries_bounded_at_ygf_two() {
    // Every attempt 401s: the hook may fire at most AWS_AUTH_MAX_ATTEMPTS
    // (Ygf=2) times, then the error goes terminal (the binary's
    // `api_request_aws_auth_exhausted` throw).
    let transport = FakeTransport::sequence(vec![FakeResponse::Ok(expired_sts_response())]);
    let aws = Arc::new(CountingAwsRefresh::default());
    let adapter = make_bedrock_adapter_with_aws(transport.clone(), aws.clone());

    let mut request =
        crate::MessagesCreateRequest::new("model", None, None, Vec::new(), Vec::new());
    request.opts.max_output_tokens = Some(4096);
    let out = adapter.messages_create(request).await;
    assert!(
        matches!(
            classified_error(&out),
            Some(LlmError::Authentication { .. })
        ),
        "exhausted refresh budget surfaces the auth error: {out:?}"
    );
    assert_eq!(
        aws.calls.load(std::sync::atomic::Ordering::SeqCst),
        crate::auth::external_aws::AWS_AUTH_MAX_ATTEMPTS,
        "refresh bounded at Ygf=2"
    );
    assert_eq!(
        transport.seen_count(),
        3,
        "initial attempt + 2 refresh retries"
    );
}

#[tokio::test]
async fn non_aws_provider_never_triggers_refresh() {
    // Provider gate (multi-provider structure is sacrosanct): the SAME 401
    // on the Anthropic first-party provider must NOT touch the refresher.
    let transport = FakeTransport::sequence(vec![FakeResponse::Ok(expired_sts_response())]);
    let aws = Arc::new(CountingAwsRefresh::default());
    let adapter = make_adapter(transport.clone());
    let adapter = adapter.with_aws_auth(aws.clone());

    let out = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;
    assert!(matches!(
        classified_error(&out),
        Some(LlmError::Authentication { .. })
    ));
    assert_eq!(
        aws.calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "refresh must never run for a non-AWS provider"
    );
    assert_eq!(transport.seen_count(), 1, "401 stays terminal, no retry");
}

#[test]
fn custom_context_beta_preserves_automatic_output_budget_intent() {
    // 520k ASCII bytes are below the default 200k input window under the
    // conservative request estimator, but leave too little headroom for a
    // full 32k output. With the 1M beta the full output allowance fits.
    let messages = vec![text_user_msg(&"x".repeat(520_000))];
    let without = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    let default_req = without
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            None,
            messages.clone(),
            Vec::new(),
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("default request");
    assert_eq!(default_req.input.max_tokens, None);

    let with_beta = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )))
    .with_custom_cli_betas(vec![
        crate::model::context_window::CONTEXT_1M_BETA_HEADER.to_string()
    ]);
    assert_eq!(
        with_beta.active_custom_betas(),
        [crate::model::context_window::CONTEXT_1M_BETA_HEADER]
    );
    let beta_req = with_beta
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            None,
            messages,
            Vec::new(),
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("1m request");
    assert_eq!(beta_req.input.max_tokens, None);
}
