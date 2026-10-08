//! Tests for `web_fetch.rs`, extracted from inline `#[cfg(test)]` blocks. Included via `#[path] mod web_fetch_test;`.

use super::*;

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn mod_replacement_validates_web_fetch_output_shape() {
        let valid = json!({
            "bytes":12,"code":200,"codeText":"OK","result":"fetched text",
            "durationMs":4,"url":"https://example.com/",
            "artifactRead":{"slug":"artifact","seeded":false}
        });
        assert!(tool_api::output_schema::validate(&OUTPUT_SCHEMA, &valid).is_ok());
        let mut invalid = valid;
        invalid["artifactRead"]["seeded"] = json!(true);
        assert!(tool_api::output_schema::validate(&OUTPUT_SCHEMA, &invalid).is_err());
    }

    #[test]
    fn accepts_https_url() {
        let u = validate_url("https://example.com/path").expect("https must be allowed");
        assert_eq!(u.scheme(), "https");
        assert_eq!(u.host_str(), Some("example.com"));
    }

    #[test]
    fn accepts_http_url() {
        let u = validate_url("http://example.com").expect("http must be allowed");
        assert_eq!(u.scheme(), "http");
    }

    // PARITY (#93): WebFetch's `validateURL` (`oqa`) has NO scheme allow-list.
    // `file:` / `data:` URLs are host-less, so they are rejected by the
    // single-label-host SSRF rule (NOT by scheme); `ftp://host.tld/` has a valid
    // 2-label host so it PASSES `validateURL` and would fail later at transport
    // time — claude-code never rejects it by scheme here.

    #[test]
    fn rejects_file_scheme_via_host_rule() {
        // `file:///etc/passwd` has an empty host (< 2 labels) → rejected by the
        // SSRF gate, exactly as claude-code's `oqa` rejects it.
        let err = validate_url("file:///etc/passwd").expect_err("file:// must be rejected");
        assert_eq!(err, "URL hostname is not publicly resolvable");
    }

    #[test]
    fn rejects_data_scheme_via_host_rule() {
        let err = validate_url("data:text/plain,hello").expect_err("data: must be rejected");
        // `data:` URLs have no host (< 2 labels) → SSRF gate rejects them.
        assert_eq!(err, "URL hostname is not publicly resolvable");
    }

    #[test]
    fn does_not_reject_ftp_scheme_by_scheme() {
        // `ftp://example.com/file` parses, has a 2-label host, and carries no
        // credentials → `validateURL` accepts it (parity: no scheme allow-list).
        // It would fail later at fetch time, not here.
        let u = validate_url("ftp://example.com/file").expect("ftp must pass validate_url");
        assert_eq!(u.scheme(), "ftp");
    }

    #[test]
    fn rejects_malformed_url() {
        let err = validate_url("not-a-url").expect_err("malformed must be rejected");
        assert!(err.starts_with("invalid URL:"));
    }

    #[test]
    fn locked_constants_match_spec() {
        // TS-faithful caps (utils.ts:112/125/128).
        assert_eq!(WEBFETCH_MAX_TRANSFER_BYTES, 10 * 1024 * 1024);
        assert_eq!(WEBFETCH_MAX_MARKDOWN_LEN, 100_000);
        assert_eq!(WEBFETCH_MAX_REDIRECTS, 10);
        assert_eq!(
            WEBFETCH_TRUNCATION_SUFFIX,
            "\n\n[Content truncated due to length...]"
        );
        assert_eq!(WEBFETCH_USER_AGENT_PREFIX, "claude-code-tool/");
        assert_eq!(WEBFETCH_ALLOWED_SCHEMES, &["https", "http"]);
    }

    #[test]
    fn tool_name_is_webfetch() {
        assert_eq!(TOOL_NAME, "WebFetch");
    }

    #[test]
    fn tool_max_result_size_matches_claude_limit() {
        let (ctx, _http, _sink) = make_web_ctx();
        let tool = WebFetchTool::new(ctx);
        assert_eq!(tool.max_result_size_chars(), 100_000);
    }

    // PARITY (#88): the body is cached/returned in FULL — there is NO
    // pre-return truncation; the 100k cap only fires inside the apply step.
    // `body_exceeds_markdown_cap` is just the source of the `truncated` flag.

    #[test]
    fn cap_flag_false_for_short_body() {
        assert!(!body_exceeds_markdown_cap("hello world"));
    }

    #[test]
    fn cap_flag_false_at_exactly_cap() {
        // length == cap is NOT over the cap (`t.length > Cut` is strict; the raw
        // fast-path also uses `length < Cut`).
        let exact = "a".repeat(WEBFETCH_MAX_MARKDOWN_LEN);
        assert!(!body_exceeds_markdown_cap(&exact));
    }

    #[test]
    fn cap_flag_true_over_cap() {
        let big = "a".repeat(WEBFETCH_MAX_MARKDOWN_LEN + 1);
        assert!(body_exceeds_markdown_cap(&big));
    }

    #[test]
    fn cap_flag_counts_utf16_units_not_bytes_bmp() {
        // BMP multibyte chars (`あ` = 3 UTF-8 bytes, 1 UTF-16 unit): the cap is
        // by UTF-16 code unit (matching JS `.length`), NOT bytes. `cap` such
        // chars is exactly at the cap (not over); `cap+1` is over.
        let at_cap = "あ".repeat(WEBFETCH_MAX_MARKDOWN_LEN);
        assert!(!body_exceeds_markdown_cap(&at_cap));
        let over = "あ".repeat(WEBFETCH_MAX_MARKDOWN_LEN + 1);
        assert!(body_exceeds_markdown_cap(&over));
    }

    #[test]
    fn cap_flag_counts_utf16_units_astral() {
        // Astral char (`😀` U+1F600 = 1 Unicode scalar, but 2 UTF-16 units).
        // JS `.length` counts 2 per emoji, so cap/2 emojis is exactly at the cap
        // and cap/2 + 1 is over — this is where UTF-16 diverges from `chars()`.
        let half = WEBFETCH_MAX_MARKDOWN_LEN / 2;
        let at_cap = "😀".repeat(half);
        assert!(!body_exceeds_markdown_cap(&at_cap));
        let over = "😀".repeat(half + 1);
        assert!(body_exceeds_markdown_cap(&over));
    }

    // ---- is_permitted_redirect (utils.ts:212-243) --------------------------

    #[test]
    fn permitted_redirect_same_host_path_change() {
        assert!(is_permitted_redirect(
            "https://example.com/a",
            "https://example.com/b?q=1"
        ));
    }

    #[test]
    fn permitted_redirect_adds_or_removes_www() {
        assert!(is_permitted_redirect(
            "https://example.com/a",
            "https://www.example.com/a"
        ));
        assert!(is_permitted_redirect(
            "https://www.example.com/a",
            "https://example.com/a"
        ));
    }

    #[test]
    fn rejected_redirect_different_host() {
        assert!(!is_permitted_redirect(
            "https://example.com/a",
            "https://evil.example.org/a"
        ));
    }

    #[test]
    fn rejected_redirect_scheme_or_port_or_creds_change() {
        // Scheme change.
        assert!(!is_permitted_redirect(
            "https://example.com/a",
            "http://example.com/a"
        ));
        // Port change.
        assert!(!is_permitted_redirect(
            "https://example.com/a",
            "https://example.com:8443/a"
        ));
        // Credentials on the redirect target.
        assert!(!is_permitted_redirect(
            "https://example.com/a",
            "https://user:pass@example.com/a"
        ));
        // Unparseable.
        assert!(!is_permitted_redirect("not-a-url", "https://example.com/a"));
    }

    #[test]
    fn format_http_error_message_matches_iip() {
        // No Retry-After (the common case): status sentence + the body note.
        assert_eq!(
            format_http_error_message(404, None),
            "The server returned HTTP 404 Not Found.\n\nThe response body was not retrieved. If this URL requires authentication, use an authenticated tool (e.g. `gh` for GitHub, or an MCP-provided fetch tool) instead of WebFetch."
        );
        assert_eq!(
            format_http_error_message(500, None),
            "The server returned HTTP 500 Internal Server Error.\n\nThe response body was not retrieved. If this URL requires authentication, use an authenticated tool (e.g. `gh` for GitHub, or an MCP-provided fetch tool) instead of WebFetch."
        );
        // With Retry-After (e.g. 429/503): a "\nRetry-After: {value}" line is
        // inserted directly after the status sentence, before the blank line.
        assert_eq!(
            format_http_error_message(429, Some("120")),
            "The server returned HTTP 429 Too Many Requests.\nRetry-After: 120\n\nThe response body was not retrieved. If this URL requires authentication, use an authenticated tool (e.g. `gh` for GitHub, or an MCP-provided fetch tool) instead of WebFetch."
        );
        // Unknown code falls through to "Unknown Status".
        assert_eq!(
            format_http_error_message(799, None),
            "The server returned HTTP 799 Unknown Status.\n\nThe response body was not retrieved. If this URL requires authentication, use an authenticated tool (e.g. `gh` for GitHub, or an MCP-provided fetch tool) instead of WebFetch."
        );
    }

    #[test]
    fn fmt_dns_error_matches_lock() {
        assert_eq!(
            fmt_dns_error("doesnotexist.invalid"),
            "WebFetch: cannot resolve doesnotexist.invalid"
        );
    }

    #[test]
    fn is_dns_failure_detects_resolve_text() {
        assert!(is_dns_failure("failed to resolve host"));
        assert!(is_dns_failure("FAILED TO RESOLVE HOST"));
        assert!(is_dns_failure("name resolution error"));
        assert!(is_dns_failure("nodename nor servname provided"));
    }

    #[test]
    fn is_dns_failure_returns_false_for_unrelated() {
        assert!(!is_dns_failure("connection refused"));
        assert!(!is_dns_failure("tls handshake failed"));
        assert!(!is_dns_failure(""));
    }

    // ---- http→https upgrade (utils.ts:406-416) -----------------------------

    #[test]
    fn upgrade_http_to_https() {
        let mut u = url::Url::parse("http://x.com/a").unwrap();
        upgrade_to_https(&mut u);
        assert_eq!(u.as_str(), "https://x.com/a");
        assert_eq!(u.scheme(), "https");
    }

    #[test]
    fn upgrade_leaves_https_untouched() {
        let mut u = url::Url::parse("https://x.com/a?q=1#frag").unwrap();
        let before = u.as_str().to_string();
        upgrade_to_https(&mut u);
        assert_eq!(u.as_str(), before);
    }

    #[test]
    fn upgrade_preserves_path_query_port() {
        let mut u = url::Url::parse("http://x.com:8080/a/b?q=1&z=2#h").unwrap();
        upgrade_to_https(&mut u);
        // url normalizes 8080 (non-default for https) — it is retained.
        assert_eq!(u.as_str(), "https://x.com:8080/a/b?q=1&z=2#h");
    }

    // ---- status reason phrase (c9n / STATUS_CODES) -------------------------

    #[test]
    fn status_reason_phrase_matches_node_table() {
        // Redirect codes used by the redirect notice.
        assert_eq!(status_reason_phrase(301), "Moved Permanently");
        assert_eq!(status_reason_phrase(308), "Permanent Redirect");
        assert_eq!(status_reason_phrase(307), "Temporary Redirect");
        assert_eq!(status_reason_phrase(302), "Found");
        // 303 is "See Other" — the prior `redirect_status_text` ternary wrongly
        // returned "Found" here (the divergence this fixes, #26).
        assert_eq!(status_reason_phrase(303), "See Other");
        // Success + client/server error phrases used by the http_error result.
        assert_eq!(status_reason_phrase(200), "OK");
        assert_eq!(status_reason_phrase(404), "Not Found");
        assert_eq!(status_reason_phrase(429), "Too Many Requests");
        assert_eq!(status_reason_phrase(418), "I'm a Teapot");
        assert_eq!(status_reason_phrase(500), "Internal Server Error");
        assert_eq!(status_reason_phrase(503), "Service Unavailable");
        // bun-only entry (Node omits 509).
        assert_eq!(status_reason_phrase(509), "Bandwidth Limit Exceeded");
        assert_eq!(status_reason_phrase(511), "Network Authentication Required");
        // Codes absent from the table fall through to "Unknown Status" (the
        // `?? "Unknown Status"` fallback), NOT "Found".
        assert_eq!(status_reason_phrase(0), "Unknown Status");
        assert_eq!(status_reason_phrase(799), "Unknown Status");
    }

    // ---- redirect message (WebFetchTool.ts:227-235) ------------------------

    #[test]
    fn format_redirect_message_byte_matches_ts() {
        // Byte-for-byte reproduction of the WebFetchTool.ts template literal.
        let expected = "REDIRECT DETECTED: The URL redirects to a different host.\n\
\n\
Original URL: https://orig.example/page\n\
Redirect URL: https://other.example/landing\n\
Status: 301 Moved Permanently\n\
\n\
To complete your request, I need to fetch content from the redirected URL. Please use WebFetch again with these parameters:\n\
- url: \"https://other.example/landing\"\n\
- prompt: \"summarize this\"";
        let got = format_redirect_message(
            "https://orig.example/page",
            "https://other.example/landing",
            301,
            "summarize this",
        );
        assert_eq!(got, expected);
    }

    #[test]
    fn format_redirect_message_uses_found_for_unknown_code() {
        let got = format_redirect_message("https://a/", "https://b/", 302, "");
        assert!(got.contains("Status: 302 Found"));
        assert!(got.contains("- prompt: \"\""));
        assert!(got.contains("- url: \"https://b/\""));
    }

    // ---- async impl Tool tests using MockHttpTransport ---------------------

    use lingxi_core::host::http::HttpTransport;
    use std::sync::Arc;
    use telemetry::sinks::InMemorySink;
    use telemetry::AnalyticsBus;
    use test_harness::mocks::{MockHttpTransport, ScriptedResponse};
    use tool_api::test_support::{fresh_ctx, fresh_tx};

    /// Serializes every test that touches the process-global
    /// `LINGXI_SKIP_WEBFETCH_PREFLIGHT` env var. The skip test *sets* it; the
    /// preflight-dependent `call()` tests *read* it (via `skip_web_fetch_preflight_env`)
    /// and would be corrupted if the skip test's mutation leaked into them while
    /// running in parallel. Mirrors the `HOME_LOCK` env-isolation idiom. A tokio
    /// mutex (not `std`) keeps the guard `Send` across the `.await` points in the
    /// async tests.
    static SKIP_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn make_web_ctx() -> (
        BuiltinToolContext,
        Arc<MockHttpTransport>,
        Arc<InMemorySink>,
    ) {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::default());
        let http = Arc::new(MockHttpTransport::new());
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            bus,
            vec![std::path::PathBuf::from("/tmp")],
        );
        ctx.http = http.clone() as Arc<dyn HttpTransport>;
        (ctx, http, sink)
    }

    fn ok_response(status: u16, body: &str) -> ScriptedResponse {
        ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
            status,
            headers: vec![],
            body: body.to_string(),
            body_bytes: Vec::new(),
        })
    }

    /// A `domain_info` preflight response that allows the fetch. The mock
    /// transport is FIFO and URL-agnostic, so the preflight GET (which the
    /// `call()` pipeline issues first, on a cache miss) consumes whatever is at
    /// the front of the queue — enqueue this *before* the fetch body response.
    fn preflight_allow() -> ScriptedResponse {
        ok_response(200, r#"{"can_fetch":true}"#)
    }

    /// Parity 2.1.207 H-BIN-03: WebFetch's `prompt()` surfaces the
    /// `claude.ai/code/artifact/{uuid}` fetch-exception ONLY when the Artifact
    /// tool's `dY()` gate is on (CC `t = await OEd(tools, null)`). With the two
    /// Statsig gates flipped on, both the SHORT and LONG variants gain the
    /// byte-exact exception wording; with them off (the default) they do not.
    #[tokio::test]
    // `dY` is the oracle's own symbol name.
    #[allow(non_snake_case)]
    async fn prompt_artifact_exception_gated_on_dY() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        telemetry::test_clear_flag("tengu_cobalt_plinth");
        telemetry::test_clear_flag("allow_cobalt_plinth");

        let (ctx, _http, _sink) = make_web_ctx();
        let tool = WebFetchTool::new(ctx);
        let long_opts = PromptOptions::default();
        let short_opts = PromptOptions {
            include_examples: false,
            model: Some("claude-opus-4-8".into()),
            model_profile: None,
            ..Default::default()
        };

        // Gate OFF (default): no exception in either variant.
        let long_off = tool.prompt(&long_opts).await;
        let short_off = tool.prompt(&short_opts).await;
        assert!(!long_off.contains("claude.ai/code/artifact/{uuid}"));
        assert!(!short_off.contains("claude.ai/code/artifact/{uuid}"));

        // Gate ON: both variants carry the byte-exact exception.
        telemetry::test_set_flag("tengu_cobalt_plinth", true);
        telemetry::test_set_flag("allow_cobalt_plinth", true);
        let long_on = tool.prompt(&long_opts).await;
        let short_on = tool.prompt(&short_opts).await;
        assert!(long_on.contains("authenticated access.\n- Exception: claude.ai/code/artifact/{uuid} URLs (including preview.claude.ai) ARE fetchable \u{2014} WebFetch uses your claude.ai login. Use WebFetch for these, not curl or a headless browser (those return the SPA shell or a Cloudflare 403, not the content).\n\n- Fetches content from a specified URL"));
        assert!(short_on.contains("use an authenticated MCP tool or `gh` for those instead. Exception: claude.ai/code/artifact/{uuid} URLs ARE fetchable via your claude.ai login \u{2014} use WebFetch, not curl (curl gets the SPA shell or a Cloudflare 403).\n- HTTP is upgraded to HTTPS."));

        telemetry::test_clear_flag("tengu_cobalt_plinth");
        telemetry::test_clear_flag("allow_cobalt_plinth");
    }

    #[tokio::test]
    async fn prompt_gates_short_vs_long_with_auth_prefix() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        // 1:1 with claude-code `CMi(model)`: a current-gen model gets the SHORT
        // variant; the default (no model) gets the LONG = `IMPORTANT: WebFetch
        // WILL FAIL…` auth-prefix + the DESCRIPTION.
        let _env = SKIP_ENV_LOCK.lock().await;
        let prev = std::env::var("LINGXI_SIMPLE_SYSTEM_PROMPT").ok();
        std::env::remove_var("LINGXI_SIMPLE_SYSTEM_PROMPT");

        let (ctx, _http, _sink) = make_web_ctx();
        let tool = WebFetchTool::new(ctx);

        // Default (model=None) ⇒ LONG: auth-prefix, then the DESCRIPTION body. The
        // `access.\n${GSd}` template (DESCRIPTION starts with `\n`) yields a blank
        // line between the prefix and the first bullet.
        let long = tool.prompt(&PromptOptions::default()).await;
        assert!(
            long.starts_with("IMPORTANT: WebFetch WILL FAIL for authenticated or private URLs."),
            "LONG must lead with the auth-warning prefix; got {long:?}"
        );
        assert!(long.contains("authenticated access.\n\n- Fetches content from a specified URL"));
        assert!(!long.contains("via Bash"));
        assert!(long.contains("authenticated GitHub tool"));

        // Current-gen model ⇒ SHORT (no auth-prefix).
        let short = tool
            .prompt(&PromptOptions {
                include_examples: false,
                model: Some("claude-opus-4-8".into()),
                model_profile: None,
                ..Default::default()
            })
            .await;
        assert!(
            short.starts_with("Fetches a URL, converts the page to markdown"),
            "SHORT variant expected; got {short:?}"
        );
        assert!(short.contains("Responses are cached for 15 minutes per URL."));
        assert!(
            !short.contains("IMPORTANT: WebFetch WILL FAIL"),
            "the SHORT variant carries no auth-warning prefix"
        );

        match prev {
            Some(v) => std::env::set_var("LINGXI_SIMPLE_SYSTEM_PROMPT", v),
            None => std::env::remove_var("LINGXI_SIMPLE_SYSTEM_PROMPT"),
        }
    }

    #[tokio::test]
    async fn http_500_returns_success_result_not_error() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        // 1:1 with claude-code: HTTP >= 400 is a SUCCESS data result carrying the
        // `iIp` body (so the model can react / fall back to gh/MCP), NOT a thrown
        // transport error.
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, sink) = make_web_ctx();
        ctx.bus.attach_sink(sink.clone()).await;
        // Preflight allows, then the fetch returns 500.
        http.enqueue(preflight_allow());
        http.enqueue(ok_response(500, "server error"));

        let tool = WebFetchTool::new(ctx);
        let result = tool
            .call(
                json!({ "url": "https://http500.example/x" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("500 must be Ok(result), not Err");
        assert_eq!(result.data["code"], 500);
        assert_eq!(result.data["codeText"], "Internal Server Error");
        assert_eq!(result.data["bytes"], 0);
        assert_eq!(
            result.data["result"].as_str().unwrap(),
            format_http_error_message(500, None)
        );
        // The tool call COMPLETED (returned a result), so `completed` fires and
        // `failed` does not.
        let events = sink.events().await;
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tengu_tool_web_fetch_started"));
        assert!(names.contains(&"tengu_tool_web_fetch_completed"));
        assert!(!names.contains(&"tengu_tool_web_fetch_failed"));
    }

    #[tokio::test]
    async fn http_429_surfaces_retry_after_header() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        // A 429 with a Retry-After header surfaces the header on its own line in
        // the result body, and reports codeText "Too Many Requests".
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, _sink) = make_web_ctx();
        http.enqueue(preflight_allow());
        http.enqueue(ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
            status: 429,
            headers: vec![("Retry-After".into(), "30".into())],
            body: String::new(),
            body_bytes: Vec::new(),
        }));
        let tool = WebFetchTool::new(ctx);
        let result = tool
            .call(
                json!({ "url": "https://ratelimited.example/x" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("429 must be Ok(result)");
        assert_eq!(result.data["code"], 429);
        assert_eq!(result.data["codeText"], "Too Many Requests");
        assert_eq!(
            result.data["result"].as_str().unwrap(),
            format_http_error_message(429, Some("30"))
        );
    }

    #[tokio::test]
    async fn http_500_does_not_retry_internally() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, _sink) = make_web_ctx();
        http.enqueue(preflight_allow());
        http.enqueue(ok_response(500, "boom"));
        let tool = WebFetchTool::new(ctx);
        let _ = tool
            .call(
                json!({ "url": "https://noretry.example/" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await;
        // One preflight + exactly one fetch (no self-retry of the 500).
        let reqs = http.received_requests();
        assert_eq!(reqs.len(), 2, "must NOT self-retry");
        assert!(reqs[0].url.contains("/api/web/domain_info?domain="));
        assert_eq!(reqs[1].url, "https://noretry.example/");
    }

    #[tokio::test]
    async fn surfaces_dns_failure() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, sink) = make_web_ctx();
        ctx.bus.attach_sink(sink.clone()).await;
        // Preflight allows; the *fetch* then fails DNS resolution.
        http.enqueue(preflight_allow());
        http.enqueue(ScriptedResponse::SyncErr(HttpError::Connection(
            "failed to resolve host doesnotexist.invalid".into(),
        )));
        let tool = WebFetchTool::new(ctx);
        let err = tool
            .call(
                json!({ "url": "https://doesnotexist.invalid/" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("DNS failure must be Err");
        match err {
            ToolError::Transport(msg) => {
                assert_eq!(msg, "WebFetch: cannot resolve doesnotexist.invalid");
            }
            other => panic!("expected Transport, got {other:?}"),
        }
        let events = sink.events().await;
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tengu_tool_web_fetch_failed"));
    }

    #[tokio::test]
    async fn happy_path_emits_completed() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, sink) = make_web_ctx();
        ctx.bus.attach_sink(sink.clone()).await;
        http.enqueue(preflight_allow());
        http.enqueue(ok_response(200, "hello world"));
        let tool = WebFetchTool::new(ctx);
        // Unique URL so the process-global cache can't be pre-warmed by another
        // parallel test (which would skip the fetch).
        let res = tool
            .call(
                json!({ "url": "https://happy.example/happy-path" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok response");
        assert_eq!(res.data["code"], 200);
        assert_eq!(res.data["result"], "hello world");
        // claude-code result schema: codeText (reason phrase) + durationMs present,
        // and NO LingXi-internal `truncated` field.
        assert_eq!(res.data["codeText"], "OK");
        assert!(res.data["durationMs"].is_u64(), "durationMs present");
        assert!(
            res.data.get("truncated").is_none(),
            "no `truncated` field (claude-code parity)"
        );
        let events = sink.events().await;
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tengu_tool_web_fetch_completed"));
    }

    #[tokio::test]
    async fn sets_user_agent_header() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, _sink) = make_web_ctx();
        http.enqueue(preflight_allow());
        http.enqueue(ok_response(200, "ok"));
        let tool = WebFetchTool::new(ctx);
        // Unique URL to avoid a process-global cache hit short-circuiting the
        // fetch (which would leave `received_requests()` empty).
        let _ = tool
            .call(
                json!({ "url": "https://useragent.example/user-agent" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await;
        // The fetch is the LAST request (the preflight has no UA header).
        let reqs = http.received_requests();
        let last_req = reqs.last().expect("captured");
        let (_, ua_value) = last_req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
            .expect("must have user-agent header");
        // R-V1: UA carries the central claude-code parity-target version, not
        // LingXi's CARGO_PKG_VERSION. Historical version literals belong in the
        // versioned parity harness, not in this live implementation test.
        assert_eq!(
            ua_value,
            &format!(
                "Claude-User (claude-code/{}; +https://support.anthropic.com/)",
                lingxi_core::host::CLAUDE_CODE_VERSION
            ),
            "WebFetch UA must be claude-code's `Claude-User (...)` form with the parity version"
        );
    }

    #[tokio::test]
    async fn rejects_file_scheme_in_call() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        // PARITY (#93): `file:///etc/passwd` is rejected by the single-label-host
        // SSRF rule (it has no host), NOT by a scheme allow-list — matching
        // claude-code's `validateURL`, which has no scheme rejection.
        let (ctx, _http, _sink) = make_web_ctx();
        let tool = WebFetchTool::new(ctx);
        let err = tool
            .call(
                json!({ "url": "file:///etc/passwd" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("file:// must be rejected");
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(format!("{err}").contains("URL hostname is not publicly resolvable"));
    }

    // ---- validateInput parity (WebFetchTool.ts:191-204) --------------------

    #[tokio::test]
    async fn validate_input_rejects_unparseable_url() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let (ctx, _http, _sink) = make_web_ctx();
        let tool = WebFetchTool::new(ctx);
        let err = tool
            .validate_input(&json!({ "url": "not a url" }), &fresh_ctx())
            .await
            .expect_err("unparseable URL must be rejected");
        // `ValidationError`'s Display prepends `invalid tool input: `; the message
        // bytes must match the TS string exactly.
        assert!(
            err.to_string().contains(
                "Error: Invalid URL \"not a url\". The URL provided could not be parsed."
            ),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn validate_input_rejects_scheme_relative_url() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        // Like JS `new URL('example.com')`, `url::Url::parse` rejects a URL with
        // no scheme/base.
        let (ctx, _http, _sink) = make_web_ctx();
        let tool = WebFetchTool::new(ctx);
        let err = tool
            .validate_input(&json!({ "url": "example.com/path" }), &fresh_ctx())
            .await
            .expect_err("schemeless URL must be rejected");
        assert!(err.to_string().contains(
            "Error: Invalid URL \"example.com/path\". The URL provided could not be parsed."
        ));
    }

    #[tokio::test]
    async fn validate_input_accepts_valid_https() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let (ctx, _http, _sink) = make_web_ctx();
        let tool = WebFetchTool::new(ctx);
        tool.validate_input(&json!({ "url": "https://example.com/page" }), &fresh_ctx())
            .await
            .expect("a parseable https URL is valid input");
    }

    #[tokio::test]
    async fn validate_input_passes_parseable_non_http_scheme() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        // Parity: validateInput only checks parseability. `file://` parses, so it
        // passes this gate — the scheme is rejected later in `call()`.
        let (ctx, _http, _sink) = make_web_ctx();
        let tool = WebFetchTool::new(ctx);
        tool.validate_input(&json!({ "url": "file:///etc/passwd" }), &fresh_ctx())
            .await
            .expect("file:// parses, so validateInput accepts it");
    }

    // ---- http→https upgrade + 15-min cache integration ---------------------

    #[tokio::test]
    async fn upgrades_http_to_https_on_the_wire() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, _sink) = make_web_ctx();
        http.enqueue(preflight_allow());
        http.enqueue(ok_response(200, "ok"));
        let tool = WebFetchTool::new(ctx);
        // Caller passes an http:// URL; the network request must target https://.
        let res = tool
            .call(
                json!({ "url": "http://upgrade.example/page" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok response");
        // Output echoes the ORIGINAL (un-upgraded) URL.
        assert_eq!(res.data["url"], "http://upgrade.example/page");
        // reqs[0] = the preflight (against the UPGRADED host); reqs[1] = the fetch.
        let reqs = http.received_requests();
        assert_eq!(reqs.len(), 2);
        assert_eq!(
            reqs[0].url,
            "https://api.anthropic.com/api/web/domain_info?domain=upgrade.example"
        );
        assert_eq!(reqs[1].url, "https://upgrade.example/page");
    }

    #[tokio::test]
    async fn second_call_is_served_from_cache() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, _sink) = make_web_ctx();
        // One preflight + one fetch for the FIRST call only. The URL-cache hit on
        // the second call short-circuits before the preflight, so no further
        // requests are issued.
        http.enqueue(preflight_allow());
        http.enqueue(ok_response(200, "cached body"));
        let tool = WebFetchTool::new(ctx);
        let url = json!({ "url": "https://cache-hit.example/doc" });

        let first = tool
            .call(url.clone(), fresh_ctx(), fresh_tx())
            .await
            .expect("first fetch ok");
        assert_eq!(first.data["result"], "cached body");

        let second = tool
            .call(url, fresh_ctx(), fresh_tx())
            .await
            .expect("second fetch ok (from cache)");
        assert_eq!(second.data["result"], "cached body");
        assert_eq!(second.data["code"], 200);
        assert_eq!(second.data["bytes"], "cached body".len());

        // First call: preflight + fetch. Second call: cache hit, zero requests.
        assert_eq!(
            http.received_requests().len(),
            2,
            "second call must hit the URL cache, not the network"
        );
    }

    #[tokio::test]
    async fn cache_keyed_by_original_url_so_http_and_https_share() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, _sink) = make_web_ctx();
        http.enqueue(preflight_allow());
        http.enqueue(ok_response(200, "body"));
        let tool = WebFetchTool::new(ctx);

        // First fetch under http:// — stored under the original (http) key.
        let _ = tool
            .call(
                json!({ "url": "http://key.example/p" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("first ok");
        // Re-fetching the same original http:// URL is a URL-cache hit.
        let _ = tool
            .call(
                json!({ "url": "http://key.example/p" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("cache hit");
        // First call: preflight + fetch. Second call: URL-cache hit, zero requests.
        assert_eq!(http.received_requests().len(), 2);
    }

    #[tokio::test]
    async fn distinct_urls_each_fetch() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, _sink) = make_web_ctx();
        // Both URLs share the host `distinct.example`. The first call runs the
        // preflight (allowing + caching the host); the second call's preflight is
        // a domain-cache hit (no request), so it issues only its fetch.
        http.enqueue(preflight_allow());
        http.enqueue(ok_response(200, "a"));
        http.enqueue(ok_response(200, "b"));
        let tool = WebFetchTool::new(ctx);
        let _ = tool
            .call(
                json!({ "url": "https://distinct.example/a" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("a ok");
        let _ = tool
            .call(
                json!({ "url": "https://distinct.example/b" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("b ok");
        // preflight (1) + two distinct URL fetches (2) = 3; the second preflight
        // is a domain-cache hit (no cross-URL content-cache hit).
        assert_eq!(http.received_requests().len(), 3);
    }

    #[tokio::test]
    async fn errors_are_not_cached() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, _sink) = make_web_ctx();
        // Preflight allows (and caches the host); both fetches then 500. If 500s
        // were content-cached, the second call would skip its fetch.
        http.enqueue(preflight_allow());
        http.enqueue(ok_response(500, "boom"));
        http.enqueue(ok_response(500, "boom"));
        let tool = WebFetchTool::new(ctx);
        let url = json!({ "url": "https://err.example/x" });
        let _ = tool.call(url.clone(), fresh_ctx(), fresh_tx()).await;
        let _ = tool.call(url, fresh_ctx(), fresh_tx()).await;
        // preflight (1, cached after) + two un-cached 500 fetches (2) = 3.
        assert_eq!(
            http.received_requests().len(),
            3,
            "failed fetches must NOT be cached"
        );
    }

    // ---- domain blocklist preflight (utils.ts:420-435) ---------------------

    #[tokio::test]
    async fn preflight_blocked_fails_with_domain_blocked_msg() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, sink) = make_web_ctx();
        ctx.bus.attach_sink(sink.clone()).await;
        // The very first request is the preflight; `can_fetch:false` blocks.
        http.enqueue(ok_response(200, r#"{"can_fetch":false}"#));
        let tool = WebFetchTool::new(ctx);
        let err = tool
            .call(
                json!({ "url": "https://blocked-host.example/page" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("blocked domain must be Err");
        match err {
            ToolError::Transport(msg) => {
                assert_eq!(msg, "LingXi is unable to fetch from blocked-host.example");
            }
            other => panic!("expected Transport, got {other:?}"),
        }
        // No fetch was attempted — only the preflight ran.
        assert_eq!(http.received_requests().len(), 1);
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.iter().any(|n| n == "tengu_tool_web_fetch_failed"));
        assert!(!names.iter().any(|n| n == "tengu_tool_web_fetch_completed"));
    }

    #[tokio::test]
    async fn preflight_non_200_fails_with_check_failed_msg() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, _sink) = make_web_ctx();
        // Preflight returns a non-200 (but no transport error) → check_failed.
        http.enqueue(ok_response(503, "service unavailable"));
        let tool = WebFetchTool::new(ctx);
        let err = tool
            .call(
                json!({ "url": "https://check-failed-503.example/page" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("non-200 preflight must be Err");
        match err {
            ToolError::Transport(msg) => {
                assert_eq!(
                    msg,
                    "Unable to verify if domain check-failed-503.example is safe to fetch. \
                     This may be due to network restrictions or enterprise security policies \
                     blocking claude.ai."
                );
            }
            other => panic!("expected Transport, got {other:?}"),
        }
        assert_eq!(http.received_requests().len(), 1);
    }

    #[tokio::test]
    async fn preflight_transport_error_fails_with_check_failed_msg() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, _sink) = make_web_ctx();
        // Preflight itself errors at the transport layer → fail-open check_failed.
        http.enqueue(ScriptedResponse::SyncErr(HttpError::Connection(
            "egress proxy refused connection".into(),
        )));
        let tool = WebFetchTool::new(ctx);
        let err = tool
            .call(
                json!({ "url": "https://check-failed-net.example/page" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("preflight transport error must be Err");
        match err {
            ToolError::Transport(msg) => {
                assert_eq!(
                    msg,
                    "Unable to verify if domain check-failed-net.example is safe to fetch. \
                     This may be due to network restrictions or enterprise security policies \
                     blocking claude.ai."
                );
            }
            other => panic!("expected Transport, got {other:?}"),
        }
        // Only the preflight ran; the fetch was never attempted.
        assert_eq!(http.received_requests().len(), 1);
    }

    #[tokio::test]
    async fn preflight_caches_allowed_host_across_distinct_urls() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, _sink) = make_web_ctx();
        // One preflight (allows + caches the host) + one fetch per distinct path.
        // The second path's preflight is a domain-cache hit — NO second domain_info.
        http.enqueue(preflight_allow());
        http.enqueue(ok_response(200, "one"));
        http.enqueue(ok_response(200, "two"));
        let tool = WebFetchTool::new(ctx);
        let _ = tool
            .call(
                json!({ "url": "https://cached-host.example/one" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("one ok");
        let _ = tool
            .call(
                json!({ "url": "https://cached-host.example/two" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("two ok");
        let reqs = http.received_requests();
        // Exactly ONE domain_info request for the host (the second path reuses
        // the 5-min domain cache).
        let preflight_count = reqs
            .iter()
            .filter(|r| r.url.contains("/api/web/domain_info?domain="))
            .count();
        assert_eq!(
            preflight_count, 1,
            "host preflight must be cached for 5 min"
        );
        // preflight (1) + two fetches (2) = 3 total.
        assert_eq!(reqs.len(), 3);
    }

    #[tokio::test]
    async fn skip_preflight_setting_issues_no_domain_info_request() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let _skip_guard = crate::testsupport::PreflightSkipGuard::set("1");
        // Sanity-check the env fallback sees the truthy value.
        assert!(skip_web_fetch_preflight_env());

        let (ctx, http, _sink) = make_web_ctx();
        // ONLY the fetch is enqueued — no preflight response. If the preflight
        // fired, it would consume this and the body assertion would fail.
        http.enqueue(ok_response(200, "no preflight here"));
        let tool = WebFetchTool::new(ctx);
        let res = tool
            .call(
                json!({ "url": "https://skip-preflight.example/page" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await;
        let _skip_guard = crate::testsupport::PreflightSkipGuard::unset();

        let res = res.expect("fetch must proceed when preflight skipped");
        assert_eq!(res.data["result"], "no preflight here");
        let reqs = http.received_requests();
        // Exactly one request — the fetch — and NO domain_info preflight.
        assert_eq!(reqs.len(), 1);
        assert!(!reqs[0].url.contains("/api/web/domain_info"));
        assert_eq!(reqs[0].url, "https://skip-preflight.example/page");
    }

    #[tokio::test]
    async fn ctx_skip_web_fetch_preflight_field_issues_no_domain_info_request() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        // parity 2.1.207 P2-14: the faithful gate is `settings.skipWebFetchPreflight`,
        // threaded via `BuiltinToolContext::skip_web_fetch_preflight`. With the env
        // fallback UNSET, setting the ctx field alone must skip the domain-blocklist
        // preflight (binary `if(!Mi().skipWebFetchPreflight){…}`).
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let _skip_guard = crate::testsupport::PreflightSkipGuard::unset();
        assert!(!skip_web_fetch_preflight_env(), "env fallback must be off");

        let (mut ctx, http, _sink) = make_web_ctx();
        ctx.skip_web_fetch_preflight = true;
        // ONLY the fetch is enqueued — no preflight response. If the preflight
        // fired, it would consume this and the body assertion would fail.
        http.enqueue(ok_response(200, "no preflight here"));
        let tool = WebFetchTool::new(ctx);
        let res = tool
            .call(
                json!({ "url": "https://ctx-skip-preflight.example/page" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("fetch must proceed when the ctx setting skips preflight");

        assert_eq!(res.data["result"], "no preflight here");
        let reqs = http.received_requests();
        // Exactly one request — the fetch — and NO domain_info preflight.
        assert_eq!(reqs.len(), 1);
        assert!(!reqs[0].url.contains("/api/web/domain_info"));
        assert_eq!(reqs[0].url, "https://ctx-skip-preflight.example/page");
    }

    #[tokio::test]
    async fn ctx_field_false_and_no_env_runs_the_preflight() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        // Negative/precedence guard: ctx field default (`false`) + env fallback
        // UNSET ⇒ the domain-blocklist preflight DOES run (a domain_info request
        // precedes the fetch), matching CC's default `!skipWebFetchPreflight` path.
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let _skip_guard = crate::testsupport::PreflightSkipGuard::unset();

        let (ctx, http, _sink) = make_web_ctx();
        assert!(!ctx.skip_web_fetch_preflight, "ctx field defaults to false");
        // Preflight allows, then the fetch returns the body.
        http.enqueue(preflight_allow());
        http.enqueue(ok_response(200, "fetched after preflight"));
        let tool = WebFetchTool::new(ctx);
        let res = tool
            .call(
                json!({ "url": "https://run-preflight.example/page" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("fetch must proceed after an allowed preflight");

        assert_eq!(res.data["result"], "fetched after preflight");
        let reqs = http.received_requests();
        // preflight (1) + fetch (1) = 2; the first request is the domain_info GET.
        assert_eq!(reqs.len(), 2);
        assert!(
            reqs[0].url.contains("/api/web/domain_info?domain="),
            "the preflight must run first: {}",
            reqs[0].url
        );
    }

    // ---- redirect loop drives request_no_follow (transport no-follow) -------

    /// A transport that distinguishes [`HttpTransport::request`] from
    /// [`HttpTransport::request_no_follow`]: it counts calls to each and only
    /// the `request_no_follow` path returns the scripted redirect/body. If the
    /// WebFetch loop regressed to calling plain `request`, the redirect response
    /// would NOT be served (the `request` arm returns a 200 sentinel and bumps a
    /// separate counter the assertions catch).
    ///
    /// Responses are FIFO from a single queue, consumed by `request_no_follow`.
    /// The blocklist preflight is skipped via `LINGXI_SKIP_WEBFETCH_PREFLIGHT`
    /// (held under `SKIP_ENV_LOCK`) so the only transport traffic is the fetch
    /// loop itself — keeping the call counts unambiguous.
    struct NoFollowMock {
        queue: std::sync::Mutex<std::collections::VecDeque<lingxi_core::types::HttpResponse>>,
        request_calls: std::sync::atomic::AtomicUsize,
        no_follow_calls: std::sync::atomic::AtomicUsize,
    }

    impl NoFollowMock {
        fn new(responses: Vec<lingxi_core::types::HttpResponse>) -> Arc<Self> {
            Arc::new(Self {
                queue: std::sync::Mutex::new(responses.into()),
                request_calls: std::sync::atomic::AtomicUsize::new(0),
                no_follow_calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl HttpTransport for NoFollowMock {
        async fn request(
            &self,
            _req: HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, HttpError> {
            // The WebFetch redirect loop must NOT reach this path. Count it and
            // return a harmless 200 so a regression is visible via the counter
            // (and the redirect/body the test scripted goes unconsumed).
            self.request_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![],
                body: "WRONG-PATH: plain request was called".into(),
                body_bytes: Vec::new(),
            })
        }
        async fn request_no_follow(
            &self,
            _req: HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, HttpError> {
            self.no_follow_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match self.queue.lock().unwrap().pop_front() {
                Some(resp) => Ok(resp),
                None => Err(HttpError::InvalidResponse("no scripted response".into())),
            }
        }
        async fn stream_sse(
            &self,
            _req: HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
            Err(HttpError::InvalidRequest(
                "sse not used in this mock".into(),
            ))
        }
    }

    fn redirect_resp(status: u16, location: &str) -> lingxi_core::types::HttpResponse {
        lingxi_core::types::HttpResponse {
            status,
            headers: vec![("location".into(), location.to_string())],
            body: String::new(),
            body_bytes: Vec::new(),
        }
    }

    fn ctx_with_transport(http: Arc<dyn HttpTransport>) -> BuiltinToolContext {
        let bus = Arc::new(AnalyticsBus::new());
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            bus,
            vec![std::path::PathBuf::from("/tmp")],
        );
        ctx.http = http;
        ctx
    }

    /// A cross-host 3xx surfaced by `request_no_follow` must drive the loop to
    /// return the byte-exact "REDIRECT DETECTED" notice — and the loop must use
    /// `request_no_follow`, NOT plain `request`. This is the production-path
    /// regression guard the whole change exists for.
    #[tokio::test]
    async fn cross_host_redirect_via_request_no_follow_returns_notice() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let _skip_guard = crate::testsupport::PreflightSkipGuard::set("1");

        let http = NoFollowMock::new(vec![redirect_resp(301, "https://other.example/landing")]);
        let ctx = ctx_with_transport(http.clone() as Arc<dyn HttpTransport>);
        let tool = WebFetchTool::new(ctx);
        let res = tool
            .call(
                json!({ "url": "https://orig.example/page", "prompt": "summarize this" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await;
        let _skip_guard = crate::testsupport::PreflightSkipGuard::unset();
        let res = res.expect("cross-host redirect must return Ok with the notice");

        assert_eq!(res.data["code"], 301);
        assert_eq!(res.data["codeText"], "Moved Permanently");
        let content = res.data["result"].as_str().unwrap();
        assert!(
            content.starts_with("REDIRECT DETECTED: The URL redirects to a different host."),
            "unexpected content: {content}"
        );
        assert!(content.contains("Redirect URL: https://other.example/landing"));
        assert!(content.contains("- prompt: \"summarize this\""));

        // The loop drove `request_no_follow`, never plain `request`.
        assert_eq!(
            http.no_follow_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "must fetch via request_no_follow"
        );
        assert_eq!(
            http.request_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "must NOT use plain request (would auto-follow in production)"
        );
    }

    /// A same-host (permitted) 3xx from `request_no_follow` must be FOLLOWED:
    /// the loop re-issues `request_no_follow` against the redirect target and
    /// returns the final body — again never touching plain `request`.
    #[tokio::test]
    async fn same_host_redirect_via_request_no_follow_is_followed() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let _skip_guard = crate::testsupport::PreflightSkipGuard::set("1");

        // First hop: same-host redirect (only the path changes). Second hop: 200.
        let http = NoFollowMock::new(vec![
            redirect_resp(301, "https://follow.example/final"),
            lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![],
                body: "final body".into(),
                body_bytes: Vec::new(),
            },
        ]);
        let ctx = ctx_with_transport(http.clone() as Arc<dyn HttpTransport>);
        let tool = WebFetchTool::new(ctx);
        let res = tool
            .call(
                json!({ "url": "https://follow.example/start" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await;
        let _skip_guard = crate::testsupport::PreflightSkipGuard::unset();
        let res = res.expect("permitted redirect must be followed to the final body");

        assert_eq!(res.data["code"], 200);
        assert_eq!(res.data["result"], "final body");
        // Two `request_no_follow` calls (start + final), zero plain `request`.
        assert_eq!(
            http.no_follow_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "must follow the permitted redirect via a second request_no_follow"
        );
        assert_eq!(
            http.request_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "must NOT use plain request"
        );
    }

    /// #87: 303 (See Other) is in claude-code's redirect set
    /// (`YHp=new Set([301,302,303,307,308])`). A cross-host 303 must be DETECTED
    /// as a redirect (→ the notice), not treated as normal page content.
    #[tokio::test]
    async fn cross_host_303_redirect_is_detected() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let _skip_guard = crate::testsupport::PreflightSkipGuard::set("1");

        let http = NoFollowMock::new(vec![redirect_resp(303, "https://other.example/landing")]);
        let ctx = ctx_with_transport(http.clone() as Arc<dyn HttpTransport>);
        let tool = WebFetchTool::new(ctx);
        let res = tool
            .call(
                json!({ "url": "https://orig.example/page", "prompt": "do x" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await;
        let _skip_guard = crate::testsupport::PreflightSkipGuard::unset();
        let res = res.expect("303 must be detected as a redirect and return Ok with the notice");

        assert_eq!(res.data["code"], 303);
        assert_eq!(res.data["codeText"], "See Other");
        let content = res.data["result"].as_str().unwrap();
        assert!(
            content.starts_with("REDIRECT DETECTED: The URL redirects to a different host."),
            "303 should produce the redirect notice, got: {content}"
        );
        assert!(content.contains("Status: 303 See Other"));
    }

    /// #92: a redirect with a missing (or empty/whitespace) Location header
    /// degrades to the `http_error` SUCCESS result with the 3xx status code
    /// (claude-code: `if(typeof l!=="string"||l.trim()==="")return
    /// {type:"http_error",statusCode:s}`), NOT a thrown transport error.
    #[tokio::test]
    async fn redirect_without_location_returns_http_error_result() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let _skip_guard = crate::testsupport::PreflightSkipGuard::set("1");

        // 301 with NO Location header.
        let http = NoFollowMock::new(vec![lingxi_core::types::HttpResponse {
            status: 301,
            headers: vec![],
            body: String::new(),
            body_bytes: Vec::new(),
        }]);
        let ctx = ctx_with_transport(http.clone() as Arc<dyn HttpTransport>);
        let tool = WebFetchTool::new(ctx);
        let res = tool
            .call(
                json!({ "url": "https://noloc.example/page" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("redirect-without-Location must be Ok(http_error result), not Err");

        assert_eq!(res.data["code"], 301);
        assert_eq!(res.data["codeText"], "Moved Permanently");
        assert_eq!(res.data["bytes"], 0);
        assert_eq!(
            res.data["result"].as_str().unwrap(),
            format_http_error_message(301, None)
        );
    }

    #[cfg(feature = "web-markdown")]
    mod markdown_apply {
        use super::*;
        use sidequery::{SideQueryClient, SideQueryError, SideQueryRequest, SideQueryResponse};
        use std::sync::atomic::{AtomicUsize, Ordering};

        pub(super) struct CapturingSideQuery {
            pub(super) captured: std::sync::Mutex<Option<String>>,
            pub(super) reply: String,
        }
        #[async_trait]
        impl SideQueryClient for CapturingSideQuery {
            async fn query(
                &self,
                request: SideQueryRequest,
            ) -> Result<SideQueryResponse, SideQueryError> {
                // `text_content()` concatenates the message's Text blocks (protocol).
                let user_text = request
                    .messages
                    .last()
                    .map(lingxi_core::types::ConversationMessage::text_content);
                *self.captured.lock().unwrap() = user_text;
                Ok(SideQueryResponse {
                    text: Some(self.reply.clone()),
                    structured: None,
                    tool_calls: vec![],
                    // Inferred as `cost::Usage::default()`. Spelled `Default::default()`
                    // so the mock needn't name `cost` (the dev-dep was dropped); the
                    // clippy::default_trait_access lint that prefers the explicit type
                    // is intentionally allowed here for that reason.
                    #[allow(clippy::default_trait_access)]
                    usage: Default::default(),
                    stop_reason: Some("end_turn".into()),
                    retry_count: 0,
                })
            }
        }

        pub(super) struct EmptySideQuery;
        #[async_trait]
        impl SideQueryClient for EmptySideQuery {
            async fn query(
                &self,
                _request: SideQueryRequest,
            ) -> Result<SideQueryResponse, SideQueryError> {
                Ok(SideQueryResponse {
                    text: None,
                    structured: None,
                    tool_calls: vec![],
                    #[allow(clippy::default_trait_access)]
                    usage: Default::default(),
                    stop_reason: Some("end_turn".into()),
                    retry_count: 0,
                })
            }
        }

        pub(super) struct FailingSideQuery;
        #[async_trait]
        impl SideQueryClient for FailingSideQuery {
            async fn query(
                &self,
                _request: SideQueryRequest,
            ) -> Result<SideQueryResponse, SideQueryError> {
                Err(SideQueryError::InvalidResponse("empty text".into()))
            }
        }

        struct CountingSideQuery {
            calls: AtomicUsize,
            reply: String,
        }

        #[async_trait]
        impl SideQueryClient for CountingSideQuery {
            async fn query(
                &self,
                _request: SideQueryRequest,
            ) -> Result<SideQueryResponse, SideQueryError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(SideQueryResponse {
                    text: Some(self.reply.clone()),
                    structured: None,
                    tool_calls: vec![],
                    #[allow(clippy::default_trait_access)]
                    usage: Default::default(),
                    stop_reason: Some("end_turn".into()),
                    retry_count: 0,
                })
            }
        }

        #[tokio::test]
        async fn fusion_policy_is_trusted_and_skips_side_query_on_miss_and_hit() {
            let _cache_guard = crate::testsupport::web_globals_lock().await;
            let _env = SKIP_ENV_LOCK.lock().await;
            crate::cache::clear_web_fetch_cache();
            crate::blocklist::clear_domain_check_cache();
            let (ctx, http, _sink) = make_web_ctx();
            http.enqueue(preflight_allow());
            http.enqueue(ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/html".into())],
                body: "<h1>Deterministic</h1><p>Body</p>".into(),
                body_bytes: Vec::new(),
            }));
            http.enqueue(preflight_allow());
            http.enqueue(ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/html".into())],
                body: "<h1>Deterministic</h1><p>Body</p>".into(),
                body_bytes: Vec::new(),
            }));
            let side_query = std::sync::Arc::new(CountingSideQuery {
                calls: AtomicUsize::new(0),
                reply: "SHOULD-NOT-RUN".into(),
            });
            let tool = WebFetchTool::new(ctx).with_side_query(side_query.clone());

            // Ordinary Agent behavior remains unchanged: one live retrieval
            // invokes the existing apply model exactly once.
            let ordinary = tool
                .call(
                    json!({
                        "url": "https://policy-boundary-ordinary.example/doc",
                        "prompt": "summarize"
                    }),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .expect("ordinary fetch succeeds");
            assert_eq!(ordinary.data["result"], "SHOULD-NOT-RUN");
            assert_eq!(side_query.calls.load(Ordering::SeqCst), 1);

            // The execution policy comes from the trusted host context. Model
            // JSON/name/query_source-looking fields cannot widen it back to
            // ordinary behavior, and both live + cache-hit paths stay local.
            let mut fusion_ctx = fresh_ctx();
            fusion_ctx.tool_execution_policy = ToolExecutionPolicy::FusionPanel;
            let mut fusion_input = json!({
                "url": "https://policy-boundary-fusion.example/doc",
                "prompt": "summarize",
                "name": "ordinary",
                "query_source": "WebFetchApply",
                "tool_execution_policy": "ordinary"
            });
            let fusion_miss = tool
                .call(fusion_input.take(), fusion_ctx.clone(), fresh_tx())
                .await
                .expect("Fusion cache miss succeeds");
            assert_eq!(fusion_miss.data["result"], "# Deterministic\n\nBody");
            assert_eq!(fusion_miss.data["source"], "network");
            assert!(fusion_miss.data["fetched_at_ms"].is_u64());
            assert_eq!(side_query.calls.load(Ordering::SeqCst), 1);

            let fusion_hit = tool
                .call(
                    json!({
                        "url": "https://policy-boundary-fusion.example/doc",
                        "prompt": "summarize",
                        "name": "ordinary",
                        "query_source": "WebFetchApply",
                        "tool_execution_policy": "ordinary"
                    }),
                    fusion_ctx,
                    fresh_tx(),
                )
                .await
                .expect("Fusion cache hit succeeds");
            assert_eq!(fusion_hit.data["result"], "# Deterministic\n\nBody");
            assert_eq!(fusion_hit.data["source"], "cache");
            assert_eq!(
                fusion_hit.data["fetched_at_ms"], fusion_miss.data["fetched_at_ms"],
                "cache hits preserve the original network fetch timestamp"
            );
            assert_eq!(side_query.calls.load(Ordering::SeqCst), 1);

            let serialized = serde_json::to_vec(&fusion_hit.data).expect("valid JSON");
            assert!(serialized.len() <= FUSION_WEBFETCH_RESULT_MAX_BYTES);
        }

        #[test]
        fn fusion_result_cap_preserves_unicode_escaping_and_provenance() {
            let body = format!("{} 😀 \\\"\n", "文".repeat(80_000));
            let data = cap_fusion_result_data(
                json!({
                    "bytes": body.len(),
                    "code": 200,
                    "codeText": "OK",
                    "result": body,
                    "durationMs": 1,
                    "url": "https://cap.example/doc"
                }),
                FetchSource::Network,
                123,
            );
            let data = data.expect("bounded Fusion result");
            let serialized = serde_json::to_vec(&data).expect("valid JSON");
            assert!(serialized.len() <= FUSION_WEBFETCH_RESULT_MAX_BYTES);
            assert_eq!(data["truncated"], true);
            assert_eq!(data["source"], "network");
            assert_eq!(data["fetched_at_ms"], 123);
            assert!(data["result"]
                .as_str()
                .expect("result string")
                .ends_with(FUSION_WEBFETCH_TRUNCATION_SUFFIX));
        }

        #[test]
        fn fusion_result_cap_fails_closed_when_metadata_alone_is_too_large() {
            let oversized_url =
                "https://example.com/".to_string() + &"x".repeat(FUSION_WEBFETCH_RESULT_MAX_BYTES);
            let err = cap_fusion_result_data(
                json!({
                    "bytes": 0,
                    "code": 200,
                    "codeText": "OK",
                    "result": "small",
                    "durationMs": 1,
                    "url": oversized_url
                }),
                FetchSource::Network,
                123,
            )
            .expect_err("metadata overflow must fail closed");
            assert!(err.contains("64KiB Fusion result limit"), "{err}");
        }

        #[tokio::test]
        async fn cache_hit_still_runs_apply_step() {
            let _env = SKIP_ENV_LOCK.lock().await;
            crate::cache::clear_web_fetch_cache();
            crate::blocklist::clear_domain_check_cache();
            let (ctx, http, _sink) = make_web_ctx();
            http.enqueue(preflight_allow());
            http.enqueue(ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/html".into())],
                body: "<h1>Doc</h1>".into(),
                body_bytes: Vec::new(),
            }));
            let capture = std::sync::Arc::new(CapturingSideQuery {
                captured: std::sync::Mutex::new(None),
                reply: "APPLIED".into(),
            });
            let tool = WebFetchTool::new(ctx).with_side_query(capture.clone());
            let url = json!({ "url": "https://cachehit-apply.example/x", "prompt": "summarize" });
            let first = tool
                .call(url.clone(), fresh_ctx(), fresh_tx())
                .await
                .expect("first ok");
            assert_eq!(first.data["result"], "APPLIED");
            let second = tool
                .call(url, fresh_ctx(), fresh_tx())
                .await
                .expect("second ok");
            assert_eq!(
                second.data["result"], "APPLIED",
                "cache hit must still run the apply step"
            );
            let seen = capture.captured.lock().unwrap().clone().unwrap();
            assert!(seen.contains("# Doc"));
            assert_eq!(
                http.received_requests().len(),
                2,
                "second call must be a cache hit (no new fetch)"
            );
        }

        // PARITY (#89): the apply step is the DEFAULT — it runs whenever a model
        // (side_query) is wired, even with NO prompt (claude-code always passes
        // the prompt `s`, empty when absent). Previously LingXi required
        // `Some(prompt)`, which was the inverse of claude-code.
        #[tokio::test]
        async fn apply_runs_even_without_a_prompt() {
            let _env = SKIP_ENV_LOCK.lock().await;
            crate::cache::clear_web_fetch_cache();
            crate::blocklist::clear_domain_check_cache();
            let (ctx, http, _sink) = make_web_ctx();
            http.enqueue(preflight_allow());
            http.enqueue(ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/html".into())],
                body: "<h1>NoPrompt</h1>".into(),
                body_bytes: Vec::new(),
            }));
            let capture = std::sync::Arc::new(CapturingSideQuery {
                captured: std::sync::Mutex::new(None),
                reply: "APPLIED".into(),
            });
            let tool = WebFetchTool::new(ctx).with_side_query(capture.clone());
            // NOTE: no "prompt" key in the input.
            let res = tool
                .call(
                    json!({ "url": "https://noprompt.example/x" }),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .expect("ok");
            assert_eq!(
                res.data["result"], "APPLIED",
                "apply must run with no prompt (#89)"
            );
        }

        // PARITY (#89): the RAW fast-path is taken ONLY when the URL is
        // preapproved AND content-type is text/markdown AND the body is under the
        // 100k cap. `docs.python.org` is a preapproved host; a small text/markdown
        // body → return raw, NO apply call.
        #[tokio::test]
        async fn preapproved_markdown_under_cap_skips_apply() {
            let _env = SKIP_ENV_LOCK.lock().await;
            crate::cache::clear_web_fetch_cache();
            crate::blocklist::clear_domain_check_cache();
            let (ctx, http, _sink) = make_web_ctx();
            http.enqueue(preflight_allow());
            http.enqueue(ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/markdown".into())],
                body: "# Raw markdown".into(),
                body_bytes: Vec::new(),
            }));
            let capture = std::sync::Arc::new(CapturingSideQuery {
                captured: std::sync::Mutex::new(None),
                reply: "SHOULD-NOT-RUN".into(),
            });
            let tool = WebFetchTool::new(ctx).with_side_query(capture.clone());
            let res = tool
                .call(
                    json!({ "url": "https://docs.python.org/3/library/os.html", "prompt": "p" }),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .expect("ok");
            assert_eq!(
                res.data["result"], "# Raw markdown",
                "preapproved+md+under-cap → raw"
            );
            assert!(
                capture.captured.lock().unwrap().is_none(),
                "apply model must NOT be called on the raw fast-path"
            );
        }

        // PARITY (#89): a preapproved + text/markdown body that is OVER the 100k
        // cap does NOT take the raw fast-path — it runs the apply step.
        #[tokio::test]
        async fn preapproved_markdown_over_cap_runs_apply() {
            let _env = SKIP_ENV_LOCK.lock().await;
            crate::cache::clear_web_fetch_cache();
            crate::blocklist::clear_domain_check_cache();
            let (ctx, http, _sink) = make_web_ctx();
            http.enqueue(preflight_allow());
            let big = "a".repeat(WEBFETCH_MAX_MARKDOWN_LEN + 1);
            http.enqueue(ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/markdown".into())],
                body: big,
                body_bytes: Vec::new(),
            }));
            let capture = std::sync::Arc::new(CapturingSideQuery {
                captured: std::sync::Mutex::new(None),
                reply: "APPLIED-OVER-CAP".into(),
            });
            let tool = WebFetchTool::new(ctx).with_side_query(capture.clone());
            let res = tool
                .call(
                    json!({ "url": "https://docs.python.org/big", "prompt": "p" }),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .expect("ok");
            assert_eq!(
                res.data["result"], "APPLIED-OVER-CAP",
                "over-cap md → apply, not raw"
            );
        }

        // PARITY (#89): a NON-preapproved text/markdown body under the cap still
        // runs the apply step (preapproval is required for the raw fast-path).
        #[tokio::test]
        async fn non_preapproved_markdown_runs_apply() {
            let _env = SKIP_ENV_LOCK.lock().await;
            crate::cache::clear_web_fetch_cache();
            crate::blocklist::clear_domain_check_cache();
            let (ctx, http, _sink) = make_web_ctx();
            http.enqueue(preflight_allow());
            http.enqueue(ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/markdown".into())],
                body: "# md".into(),
                body_bytes: Vec::new(),
            }));
            let capture = std::sync::Arc::new(CapturingSideQuery {
                captured: std::sync::Mutex::new(None),
                reply: "APPLIED-NONPRE".into(),
            });
            let tool = WebFetchTool::new(ctx).with_side_query(capture.clone());
            let res = tool
                .call(
                    json!({ "url": "https://random-not-preapproved.example/x", "prompt": "p" }),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .expect("ok");
            assert_eq!(
                res.data["result"], "APPLIED-NONPRE",
                "non-preapproved md → apply"
            );
        }

        #[tokio::test]
        async fn empty_apply_response_falls_back_to_markdown() {
            let _env = SKIP_ENV_LOCK.lock().await;
            crate::cache::clear_web_fetch_cache();
            crate::blocklist::clear_domain_check_cache();
            let (ctx, http, _sink) = make_web_ctx();
            http.enqueue(preflight_allow());
            http.enqueue(ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/html".into())],
                body: "<h1>Fallback</h1>".into(),
                body_bytes: Vec::new(),
            }));
            let tool = WebFetchTool::new(ctx).with_side_query(std::sync::Arc::new(EmptySideQuery));
            let res = tool
                .call(
                    json!({ "url": "https://empty-apply.example/x", "prompt": "summarize" }),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .expect("ok");
            assert_eq!(res.data["result"], "# Fallback");
        }

        #[tokio::test]
        async fn apply_error_falls_back_to_markdown() {
            let _env = SKIP_ENV_LOCK.lock().await;
            crate::cache::clear_web_fetch_cache();
            crate::blocklist::clear_domain_check_cache();
            let (ctx, http, _sink) = make_web_ctx();
            http.enqueue(preflight_allow());
            http.enqueue(ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/html".into())],
                body: "<h1>Recovered</h1>".into(),
                body_bytes: Vec::new(),
            }));
            let tool =
                WebFetchTool::new(ctx).with_side_query(std::sync::Arc::new(FailingSideQuery));
            let res = tool
                .call(
                    json!({ "url": "https://failed-apply.example/x", "prompt": "summarize" }),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .expect("ok");
            assert_eq!(res.data["result"], "# Recovered");
        }
    }

    #[cfg(feature = "web-markdown")]
    #[tokio::test]
    async fn apply_step_runs_model_over_markdown() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, _sink) = make_web_ctx();
        http.enqueue(preflight_allow());
        http.enqueue(ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/html".into())],
            body: "<h1>Title</h1><p>Body text</p>".into(),
            body_bytes: Vec::new(),
        }));
        let capture = std::sync::Arc::new(markdown_apply::CapturingSideQuery {
            captured: std::sync::Mutex::new(None),
            reply: "MODEL SUMMARY".into(),
        });
        let tool = WebFetchTool::new(ctx).with_side_query(capture.clone());
        let res = tool
            .call(
                json!({ "url": "https://apply.example/x", "prompt": "summarize" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(res.data["result"], "MODEL SUMMARY");
        let seen = capture.captured.lock().unwrap().clone().unwrap();
        assert!(
            seen.contains("# Title"),
            "model prompt should carry markdown: {seen}"
        );
        assert!(seen.contains("summarize"));
        assert!(!seen.contains("<h1>"), "HTML must be converted, not raw");
    }

    #[cfg(feature = "web-markdown")]
    #[tokio::test]
    async fn no_side_query_returns_markdown_unchanged_behavior() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, _sink) = make_web_ctx();
        http.enqueue(preflight_allow());
        http.enqueue(ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/html".into())],
            body: "<h1>Hi</h1>".into(),
            body_bytes: Vec::new(),
        }));
        let tool = WebFetchTool::new(ctx);
        let res = tool
            .call(
                json!({ "url": "https://nomarkdown.example/x", "prompt": "q" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(res.data["result"], "# Hi");
    }

    #[test]
    fn with_side_query_sets_the_client() {
        let (ctx, _http, _sink) = make_web_ctx();
        let tool = WebFetchTool::new(ctx);
        assert!(
            tool.side_query.is_none(),
            "default has no side-query client"
        );
    }

    #[test]
    fn skip_preflight_env_parsing() {
        // Must take the SAME lock the parallel `call()` tests take. This used
        // to take SKIP_ENV_LOCK, which guards LINGXI_SIMPLE_SYSTEM_PROMPT, and
        // claimed in a comment that it therefore could not race them.
        let _env = crate::testsupport::block_on_web_globals_lock();
        for truthy in ["1", "true", "TRUE", "Yes", "on", " on "] {
            let _skip_guard = crate::testsupport::PreflightSkipGuard::set(truthy);
            assert!(skip_web_fetch_preflight_env(), "{truthy:?} must be truthy");
        }
        for falsy in ["0", "false", "no", "off", "", "garbage"] {
            let _skip_guard = crate::testsupport::PreflightSkipGuard::set(falsy);
            assert!(!skip_web_fetch_preflight_env(), "{falsy:?} must be falsy");
        }
        let _skip_guard = crate::testsupport::PreflightSkipGuard::unset();
        assert!(!skip_web_fetch_preflight_env(), "unset must be falsy");
    }

    /// Build a web ctx whose workspace is `workspace` (so binary-persist writes
    /// land in a controlled temp dir, not `/tmp`).
    fn make_web_ctx_with_workspace(
        workspace: std::path::PathBuf,
    ) -> (
        BuiltinToolContext,
        Arc<MockHttpTransport>,
        Arc<InMemorySink>,
    ) {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::default());
        let http = Arc::new(MockHttpTransport::new());
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            bus,
            vec![workspace.clone()],
        );
        ctx.http = http.clone() as Arc<dyn HttpTransport>;
        ctx.session_cwd.swap(workspace, ctx.trusted_dirs());
        (ctx, http, sink)
    }

    // PARITY (#88): a body OVER the 100k cap is cached + returned in FULL (no
    // pre-return truncation, no truncation suffix on the raw path) — the cap only
    // fires inside the apply step, which is absent here (no side_query). The
    // `truncated` flag is still set.
    #[tokio::test]
    async fn oversized_body_is_returned_in_full_no_apply() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let (ctx, http, _sink) = make_web_ctx();
        let big = "Z".repeat(WEBFETCH_MAX_MARKDOWN_LEN + 5000);
        http.enqueue(preflight_allow());
        http.enqueue(ok_response(200, &big));
        let tool = WebFetchTool::new(ctx);
        let res = tool
            .call(
                json!({ "url": "https://full-body.example/big" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        let content = res.data["result"].as_str().unwrap();
        // FULL body, NOT truncated, NO suffix.
        assert_eq!(content.chars().count(), WEBFETCH_MAX_MARKDOWN_LEN + 5000);
        assert!(!content.ends_with(WEBFETCH_TRUNCATION_SUFFIX));
        // The cache also holds the FULL body.
        let cached = crate::cache::cache_get("https://full-body.example/big").expect("cached");
        assert_eq!(
            cached.content.chars().count(),
            WEBFETCH_MAX_MARKDOWN_LEN + 5000
        );
    }

    // PARITY (#94): a binary content-type persists the body to a temp file and
    // appends the `[Binary content (...) also saved to <path>]` footer. (No
    // side_query → raw body + footer.)
    #[tokio::test]
    async fn binary_content_persists_and_appends_footer() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (ctx, http, _sink) = make_web_ctx_with_workspace(tmp.path().to_path_buf());
        let body = "%PDF-1.4 fake pdf bytes";
        http.enqueue(preflight_allow());
        http.enqueue(ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/pdf".into())],
            body: body.to_string(),
            body_bytes: Vec::new(),
        }));
        let tool = WebFetchTool::new(ctx);
        let res = tool
            .call(
                json!({ "url": "https://binary.example/file.pdf" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        let content = res.data["result"].as_str().unwrap();
        // Raw body, then the binary footer.
        assert!(content.starts_with(body));
        assert!(
            content.contains(&format!(
                "\n\n[Binary content (application/pdf, {}) also saved to ",
                crate::persist::human_size(body.len() as u64)
            )),
            "missing binary footer: {content}"
        );
        // NOTE: this asserts the NO-SESSION FALLBACK, not the production path.
        // `ctx_for_file_tools` leaves `session_id` unset, so
        // `BuiltinToolContext::tool_results_dir` returns the workspace-local dir.
        // With a session — which the live desktop context always sets — output
        // goes to `<config>/projects/<cwd>/<session-id>/tool-results` instead
        // (claude-code `xke()`); that branch is covered in `tool-api`.
        // The persisted file exists under <workspace>/.lingxi/tool-results with a
        // .pdf extension.
        let results_dir = tmp.path().join(".lingxi").join("tool-results");
        let entries: Vec<_> = std::fs::read_dir(&results_dir)
            .expect("tool-results dir created")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(entries.len(), 1, "exactly one persisted artifact");
        let name = entries[0].file_name().to_string_lossy().into_owned();
        assert!(name.starts_with("webfetch-"), "name: {name}");
        assert_eq!(
            std::path::Path::new(&name)
                .extension()
                .and_then(|e| e.to_str()),
            Some("pdf"),
            "name: {name}"
        );
        assert_eq!(std::fs::read(entries[0].path()).unwrap(), body.as_bytes());
    }

    // PARITY (P3-ALL): a genuinely-binary body (invalid UTF-8) must be persisted
    // BYTE-IDENTICALLY to the wire. The transport carries the raw bytes in
    // `HttpResponse.body_bytes` (populated by reqwest's `resp.bytes()` —
    // claude-code's `responseType:"arraybuffer"`); the lossy `body` String is
    // NOT byte-identical, so persisting `body.as_bytes()` would corrupt the
    // artifact. This pins that WebFetch persists the raw `body_bytes`.
    #[tokio::test]
    async fn binary_body_persists_raw_bytes_not_lossy_string() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (ctx, http, _sink) = make_web_ctx_with_workspace(tmp.path().to_path_buf());
        // `%PDF-1.4` then NUL + invalid-UTF8 bytes (0xFF 0xFE 0x89) — these are
        // replaced by U+FFFD under lossy decoding, so the String differs from
        // the wire both in bytes and in length.
        let raw: Vec<u8> = vec![
            0x25, 0x50, 0x44, 0x46, 0x2D, 0x31, 0x2E, 0x34, 0x00, 0xFF, 0xFE, 0x89,
        ];
        let lossy = String::from_utf8_lossy(&raw).into_owned();
        assert_ne!(
            lossy.as_bytes(),
            raw.as_slice(),
            "sanity: lossy String must differ from the raw wire bytes"
        );
        http.enqueue(preflight_allow());
        http.enqueue(ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/pdf".into())],
            body: lossy,
            body_bytes: raw.clone(),
        }));
        let tool = WebFetchTool::new(ctx);
        let res = tool
            .call(
                json!({ "url": "https://binary.example/raw.pdf" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        // The reported byte count is the RAW wire length (arraybuffer byteLength).
        assert_eq!(res.data["bytes"], raw.len());
        // The footer shows the raw byte size.
        let content = res.data["result"].as_str().unwrap();
        assert!(
            content.contains(&format!(
                "\n\n[Binary content (application/pdf, {}) also saved to ",
                crate::persist::human_size(raw.len() as u64)
            )),
            "missing binary footer: {content}"
        );
        // The persisted artifact is byte-identical to the WIRE, not to the lossy
        // String — the whole point of P3-ALL.
        let results_dir = tmp.path().join(".lingxi").join("tool-results");
        let entries: Vec<_> = std::fs::read_dir(&results_dir)
            .expect("tool-results dir created")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(entries.len(), 1, "exactly one persisted artifact");
        let saved = std::fs::read(entries[0].path()).unwrap();
        assert_eq!(saved, raw, "persisted artifact must be the raw wire bytes");
        assert_ne!(
            saved,
            String::from_utf8_lossy(&raw).into_owned().into_bytes(),
            "persisted artifact must NOT be the lossy-decoded String bytes"
        );
    }

    // PARITY (#94): a NON-binary (text/html) body does NOT persist and gets NO
    // footer.
    #[tokio::test]
    async fn text_html_does_not_persist_or_footer() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        let _env = SKIP_ENV_LOCK.lock().await;
        crate::cache::clear_web_fetch_cache();
        crate::blocklist::clear_domain_check_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (ctx, http, _sink) = make_web_ctx_with_workspace(tmp.path().to_path_buf());
        http.enqueue(preflight_allow());
        http.enqueue(ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/html; charset=utf-8".into())],
            body: "<p>hi</p>".into(),
            body_bytes: Vec::new(),
        }));
        let tool = WebFetchTool::new(ctx);
        let res = tool
            .call(
                json!({ "url": "https://text.example/page" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        let content = res.data["result"].as_str().unwrap();
        assert!(!content.contains("[Binary content"), "no footer for text/*");
        assert!(
            !tmp.path().join(".lingxi").join("tool-results").exists(),
            "no tool-results dir created for non-binary content"
        );
    }
}
